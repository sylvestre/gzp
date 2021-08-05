//! Parallel Gzip compression.
//!
//! This modeul provides a an implementation of [`Write`] that is backed by an async threadpool that
//! which compresses blocks and writes to the underlying writer. This is very similar to how
//! [`pigz`](https://zlib.net/pigz/) works.
//!
//! # References
//!
//! - [ParallelGzip](https://github.com/shevek/parallelgzip/blob/master/src/main/java/org/anarres/parallelgzip/ParallelGZIPOutputStream.java)
//! - [pigz](https://zlib.net/pigz/)
//!
//! # Known Differences from Pigz
//!
//! - Each block has an independent CRC value
//! - There is no continual dictionary for compression, compression is per-block only. On some data
//!   types this could lead to no compression for a given block if the block is small enough or the
//!   data is random enough.
//!
//! # Examples
//!
//! ```
//! use std::{env, fs::File, io::Write};
//!
//! use gzp::ParGz;
//!
//! fn main() {
//!     let mut writer = vec![];
//!     let mut par_gz = ParGz::builder(writer).build();
//!     par_gz.write_all(b"This is a first test line\n").unwrap();
//!     par_gz.write_all(b"This is a second test line\n").unwrap();
//!     par_gz.finish().unwrap();
//! }
//! ```
use std::io::{self, Read, Write};

use bytes::BytesMut;
use flate2::bufread::GzEncoder;
pub use flate2::Compression;
use futures::executor::block_on;
use thiserror::Error;
use tokio::sync::mpsc::{self, Receiver, Sender};

/// 128 KB default buffer size, same as pigz
const BUFSIZE: usize = 64 * (1 << 10) * 2;

/// The [`ParGz`] builder.
#[derive(Debug)]
pub struct ParGzBuilder<W> {
    /// The level to compress the output. Defaults to `3`.
    compression_level: Compression,
    /// The buffersize accumulate before trying to compress it. Defaults to [`BUFSIZE`].
    buffer_size: usize,
    /// The underlying writer to write to.
    writer: W,
    /// The number of threads to use for compression. Defaults to all available threads.
    num_threads: usize,
}

impl<W> ParGzBuilder<W>
where
    W: Send + Write + 'static,
{
    /// Create a new [`ParGzBuilder`] object.
    pub fn new(writer: W) -> Self {
        Self {
            compression_level: Compression::new(3),
            buffer_size: BUFSIZE,
            writer,
            num_threads: num_cpus::get(),
        }
    }

    /// Set the [`buffer_size`](ParGzBuilder.buffer_size).
    pub fn buffer_size(mut self, buffer_size: usize) -> Self {
        assert!(buffer_size > 0);
        self.buffer_size = buffer_size;
        self
    }

    /// Set the [`compression_level`](ParGzBuilder.compression_level).
    pub fn compression_level(mut self, compression_level: Compression) -> Self {
        self.compression_level = compression_level;
        self
    }

    /// Set the [`num_threads`](ParGzBuilder.num_threads).
    pub fn num_threads(mut self, num_threads: usize) -> Self {
        assert!(num_threads <= num_cpus::get() && num_threads > 0);
        self.num_threads = num_threads;
        self
    }

    /// Create a configured [`ParGz`] object.
    pub fn build(self) -> ParGz {
        let (tx, rx) = mpsc::channel(self.num_threads);
        let buffer_size = self.buffer_size;
        let handle = std::thread::spawn(move || {
            ParGz::run(rx, self.writer, self.num_threads, self.compression_level)
        });
        ParGz {
            handle,
            tx,
            buffer: BytesMut::with_capacity(buffer_size),
            buffer_size,
        }
    }
}

#[derive(Error, Debug)]
pub enum ParGzError {
    #[error("Failed to send over channel.")]
    ChannelSend,
    #[error(transparent)]
    Io(#[from] io::Error),
    #[error(transparent)]
    Join(#[from] tokio::task::JoinError),
    #[error("Unknown")]
    Unknown,
}

pub struct ParGz {
    handle: std::thread::JoinHandle<Result<(), ParGzError>>,
    tx: Sender<BytesMut>,
    buffer: BytesMut,
    buffer_size: usize,
}

impl ParGz {
    /// Create a builder to configure the [`ParGz`] runtime.
    pub fn builder<W>(writer: W) -> ParGzBuilder<W>
    where
        W: Write + Send + 'static,
    {
        ParGzBuilder::new(writer)
    }

    /// Launch the tokio runtime that coordinates the threadpool that does the following:
    ///
    /// 1. Receives chunks of bytes from from the [`ParGz::write`] method.
    /// 2. Spawn a task compressing the chunk of bytes.
    /// 3. Send the future for that task to the writer.
    /// 4. Write the bytes to the underlying writer.
    fn run<W>(
        mut rx: Receiver<BytesMut>,
        mut writer: W,
        num_threads: usize,
        compression_level: Compression,
    ) -> Result<(), ParGzError>
    where
        W: Write + Send + 'static,
    {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(num_threads)
            .build()?;

        // Spawn the main task
        rt.block_on(async {
            let (out_sender, mut out_receiver) = mpsc::channel(num_threads);
            let compressor = tokio::task::spawn(async move {
                while let Some(chunk) = rx.recv().await {
                    let task =
                        tokio::task::spawn_blocking(move || -> Result<Vec<u8>, ParGzError> {
                            let mut buffer = Vec::with_capacity(chunk.len());
                            let mut gz: GzEncoder<&[u8]> =
                                GzEncoder::new(&chunk[..], compression_level);
                            gz.read_to_end(&mut buffer)?;

                            Ok(buffer)
                        });
                    out_sender
                        .send(task)
                        .await
                        .map_err(|_e| ParGzError::ChannelSend)?;
                }
                Ok::<(), ParGzError>(())
            });

            let writer_task = tokio::task::spawn_blocking(move || -> Result<(), ParGzError> {
                while let Some(chunk) = block_on(out_receiver.recv()) {
                    let chunk = block_on(chunk)??;
                    writer.write_all(&chunk)?;
                }
                writer.flush()?;
                Ok(())
            });

            compressor.await??;
            writer_task.await??;
            Ok::<(), ParGzError>(())
        })
    }

    /// Flush the buffers and wait on all threads to finish working.
    ///
    /// This *MUST* be called before the [`ParGz`] object goes out of scope.
    pub fn finish(mut self) -> Result<(), ParGzError> {
        self.flush()?;
        drop(self.tx);
        self.handle.join().unwrap()
    }
}

impl Write for ParGz {
    /// Write a buffer into this writer, returning how many bytes were written.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(buf);
        if self.buffer.len() > self.buffer_size {
            let b = self.buffer.split_to(self.buffer_size);
            block_on(self.tx.send(b)).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            self.buffer
                .reserve(self.buffer_size.saturating_sub(self.buffer.len()))
        }

        Ok(buf.len())
    }

    /// Flush this output stream, ensuring all intermediately buffered contents are sent.
    fn flush(&mut self) -> std::io::Result<()> {
        block_on(self.tx.send(self.buffer.split()))
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use std::{
        fs::File,
        io::{BufReader, BufWriter},
    };

    use flate2::bufread::MultiGzDecoder;
    use proptest::prelude::*;
    use tempfile::tempdir;

    use super::*;

    #[test]
    fn test_simple() {
        let dir = tempdir().unwrap();

        // Create output file
        let output_file = dir.path().join("output.txt");
        let out_writer = BufWriter::new(File::create(&output_file).unwrap());

        // Define input bytes
        let input = b"
        This is a longer test than normal to come up with a bunch of text.
        We'll read just a few lines at a time.
        ";

        // Compress input to output
        let mut par_gz = ParGz::builder(out_writer).build();
        par_gz.write_all(input).unwrap();
        par_gz.finish().unwrap();

        // Read output back in
        let mut reader = BufReader::new(File::open(output_file).unwrap());
        let mut result = vec![];
        reader.read_to_end(&mut result).unwrap();

        // Decompress it
        let mut gz = MultiGzDecoder::new(&result[..]);
        let mut bytes = vec![];
        gz.read_to_end(&mut bytes).unwrap();

        // Assert decompressed output is equal to input
        assert_eq!(input.to_vec(), bytes);
    }

    #[test]
    fn test_regression() {
        let dir = tempdir().unwrap();

        // Create output file
        let output_file = dir.path().join("output.txt");
        let out_writer = BufWriter::new(File::create(&output_file).unwrap());

        // Define input bytes that is 206 bytes long
        let input = [
            132, 19, 107, 159, 69, 217, 180, 131, 224, 49, 143, 41, 194, 30, 151, 22, 55, 30, 42,
            139, 219, 62, 123, 44, 148, 144, 88, 233, 199, 126, 110, 65, 6, 87, 51, 215, 17, 253,
            22, 63, 110, 1, 100, 202, 44, 138, 187, 226, 50, 50, 218, 24, 193, 218, 43, 172, 69,
            71, 8, 164, 5, 186, 189, 215, 151, 170, 243, 235, 219, 103, 1, 0, 102, 80, 179, 95,
            247, 26, 168, 147, 139, 245, 177, 253, 94, 82, 146, 133, 103, 223, 96, 34, 128, 237,
            143, 182, 48, 201, 201, 92, 29, 172, 137, 70, 227, 98, 181, 246, 80, 21, 106, 175, 246,
            41, 229, 187, 87, 65, 79, 63, 115, 66, 143, 251, 41, 251, 214, 7, 64, 196, 27, 180, 42,
            132, 116, 211, 148, 44, 177, 137, 91, 119, 245, 156, 78, 24, 253, 69, 38, 52, 152, 115,
            123, 94, 162, 72, 186, 239, 136, 179, 11, 180, 78, 54, 217, 120, 173, 141, 114, 174,
            220, 160, 223, 184, 114, 73, 148, 120, 43, 25, 21, 62, 62, 244, 85, 87, 19, 174, 182,
            227, 228, 70, 153, 5, 92, 51, 161, 9, 140, 199, 244, 241, 151, 236, 81, 211,
        ];

        // Compress input to output
        let mut par_gz = ParGz::builder(out_writer)
            .buffer_size(205)
            .num_threads(3)
            .compression_level(Compression::new(2))
            .build();
        par_gz.write_all(&input).unwrap();
        par_gz.finish().unwrap();

        // Read output back in
        let mut reader = BufReader::new(File::open(output_file).unwrap());
        let mut result = vec![];
        reader.read_to_end(&mut result).unwrap();

        // Decompress it
        let mut gz = MultiGzDecoder::new(&result[..]);
        let mut bytes = vec![];
        gz.read_to_end(&mut bytes).unwrap();

        // Assert decompressed output is equal to input
        assert_eq!(input.to_vec(), bytes);
    }

    proptest! {
        #[test]
        fn test_all(
            input in prop::collection::vec(0..u8::MAX, 1..10_000),
            buf_size in 1..10_000usize,
            comp_lvl in 0..9u32,
            num_threads in 1..num_cpus::get()
        ) {
        let dir = tempdir().unwrap();

        // Create output file
        let output_file = dir.path().join("output.txt");
        let out_writer = BufWriter::new(File::create(&output_file).unwrap());


        // Compress input to output
        let mut par_gz = ParGz::builder(out_writer)
            .buffer_size(buf_size)
            .compression_level(Compression::new(comp_lvl))
            .num_threads(num_threads)
            .build();
        par_gz.write_all(&input).unwrap();
        par_gz.finish().unwrap();

        // Read output back in
        let mut reader = BufReader::new(File::open(output_file).unwrap());
        let mut result = vec![];
        reader.read_to_end(&mut result).unwrap();

        // Decompress it
        let mut gz = MultiGzDecoder::new(&result[..]);
        let mut bytes = vec![];
        gz.read_to_end(&mut bytes).unwrap();

        // Assert decompressed output is equal to input
        assert_eq!(input.to_vec(), bytes);
        }
    }
}