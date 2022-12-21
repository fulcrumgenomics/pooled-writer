//! A pooled writer and compressor.
//!
//! # Overview
//!
//! `pooled-writer` solves the problem of compressing and writing data to a set of writers using
//! multiple threads, where the number of writers and threads cannot easily be equal.  For example
//! writing to hundreds of gzipped files using 16 threads, or writing to a four gzipped files
//! using 32 threads.
//!
//! To accomplish this, a pool is configured and writers are exchanged for [`PooledWriter`]s
//! that can be used in place of the original writers.  This is accomplished using the
//! [`PoolBuilder`] which is the preferred way to configure and create a pool.  The [`Pool`] and
//! builder require two generic types: the `W` Writer type and the `C` compressor type. `W` may
//! usually be elided if calls to [`PoolBuilder::exchange`] may be used to infer the type. `C`
//! must be specified as something that implements [`Compressor`].
//!
//! The [`Pool`] consists of two thread pools, one for compressing and one for writing. All
//! concurrency is managed via message passing over channels.
//!
//! Every time the internal buffer of a [`PooledWriter`] reaches capacity (defined by
//! [`Compressor::BLOCK_SIZE`]) it sends two messages:
//! 1. It sends a message over the corresponding writer's channel to the writer pool, enqueueing
//!    a one-shot receiver channel in the writers queue that will receive the compressed bytes
//!    once the compressor is done. This is done to maintain the output order.
//! 2. It sends a message to the compressor pool that contains a buffer of bytes to compress
//!    as well as the sender side of the one-shot channel to send the compressed bytes on.
//!
//! The writer thread pool contains a `Vec` of receivers, one for each writer. It loops over
//! this `Vec`, checking to see if the receiver has any messages. If it does, a lock is
//! acquired and that writer's receiver is drained, writing to the underlying writer that was exchanged
//! for the [`PooledWriter`].
//!
//! The compressor thread pool consists of a single receiver that is continually polled for new
//! messages. The messages are processed, the bytes compressed, and then the compressed bytes are
//! sent over the one-shot channel to the corresponding receiver, which is a place-holder receiver
//! in the writer queues.
//!
//! Shutdown of the entire pool is managed via a sentinel value that is checked in the writer loop.
//! If a shutdown has been requested a cascade of channel drops will cleanly disconnect all senders
//! and receivers and any further calls to [`PooledWriter`]s will result in an error.
//!
//! # Example
//!
//! ```rust
//! use std::{
//!     error::Error,
//!     fs::File,
//!     io::{BufWriter, Write},
//!     path::Path,
//! };
//!
//! use pooled_writer::{Compressor, PoolBuilder, Pool, bgzf::BgzfCompressor};
//!
//! type DynError = Box<dyn Error + 'static>;
//!
//! fn create_writer<P: AsRef<Path>>(name: P) -> Result<BufWriter<File>, DynError> {
//!     Ok(BufWriter::new(File::create(name)?))
//! }
//!
//! fn main() -> Result<(), DynError> {
//!     let writers = vec![
//!         create_writer("/tmp/test1.txt.gz")?,
//!         create_writer("/tmp/test2.txt.gz")?,
//!         create_writer("/tmp/test3.txt.gz")?,
//!     ];
//!
//!     let mut builder = PoolBuilder::<_, BgzfCompressor>::new(20, 8)
//!         .compression_level(5)?;
//!
//!    let mut pooled_writers = writers.into_iter().map(|w| builder.exchange(w)).collect::<Vec<_>>();
//!    let mut pool = builder.build()?;
//!
//!     writeln!(&mut pooled_writers[1], "This is writer2")?;
//!     writeln!(&mut pooled_writers[0], "This is writer1")?;
//!     writeln!(&mut pooled_writers[2], "This is writer3")?;
//!     pooled_writers.into_iter().try_for_each(|w| w.close())?;
//!     pool.stop_pool()?;
//!
//!     Ok(())
//! }
//! ```
#![forbid(unsafe_code)]
#![allow(
    unused,
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::module_name_repetitions
)]

#[cfg(feature = "bgzf_compressor")]
pub mod bgzf;

use std::time::Duration;
use std::{
    error::Error,
    io::{self, Read, Write},
    sync::Arc,
    thread::JoinHandle,
};

use bytes::{Bytes, BytesMut};
use flume::{self, Receiver, Sender};
use parking_lot::{lock_api::RawMutex, Mutex};
use thiserror::Error;

/// 128 KB default buffer size, same as pigz.
pub(crate) const BUFSIZE: usize = 128 * 1024;

/// Convenience type for functions that return [`PoolError`].
type PoolResult<T> = Result<T, PoolError>;

/// Represents errors that may be generated by any `Pool` related functionality.
#[non_exhaustive]
#[derive(Error, Debug)]
pub enum PoolError {
    #[error("Failed to send over channel")]
    ChannelSend,
    #[error(transparent)]
    ChannelReceive(#[from] flume::RecvError),

    // TODO: figure out how to better pass in an generic / dynamic error type to this.
    #[error("Error compressing data: {0}")]
    CompressionError(String),
    #[error(transparent)]
    Io(#[from] io::Error),
}

////////////////////////////////////////////////////////////////////////////////
// The PooledWriter and it's impls
////////////////////////////////////////////////////////////////////////////////

/// A [`PooledWriter`] is created by exchanging a writer with a [`Pool`].
///
/// The pooled writer will internally buffer writes, sending bytes to the [`Pool`]
/// after the internal buffer has been filled.
///
/// Note that the `compressor_tx` channel is shared by all pooled writers, whereas the `writer_tx`
/// is specific to the _underlying_ writer that this pooled writer encapsulates.
#[derive(Debug)]
pub struct PooledWriter {
    /// The index/serial number of the pooled writer within the pool
    writer_index: usize,
    /// Channel to send messages containing bytes to compress to the compressors' pool.
    compressor_tx: Sender<CompressorMessage>,
    /// Channel to send the receiving end of the one-shot channel that will be
    /// used to send the compressed bytes. This effectively "place holds" the
    /// position of the compressed bytes in the writers queue until the compressed bytes
    /// are ready.
    writer_tx: Sender<Receiver<WriterMessage>>,
    /// The internal buffer to gather bytes to send.
    buffer: BytesMut,
    /// The desired size of the internal buffer.
    buffer_size: usize,
}

impl PooledWriter {
    /// Create a new [`PooledWriter`] that has an internal buffer capacity that matches [`bgzf::BGZF_BLOCK_SIZE`].
    ///
    /// # Arguments
    /// - `index` - a usize representing that this is the nth pooled writer created within the pool
    /// - `compressor_tx` - The channel to send uncompressed bytes to the compressor pool.
    /// - `writer_tx` - The `Send` end of the channel that transmits the `Receiver` end of the one-shot
    ///                 channel, which will be consumed when the compressor sends the compressed bytes.
    fn new<C>(
        index: usize,
        compressor_tx: Sender<CompressorMessage>,
        writer_tx: Sender<Receiver<WriterMessage>>,
    ) -> Self
    where
        C: Compressor,
    {
        Self {
            writer_index: index,
            compressor_tx,
            writer_tx,
            buffer: BytesMut::with_capacity(C::BLOCK_SIZE),
            buffer_size: C::BLOCK_SIZE,
        }
    }

    /// Test whether the internal buffer has reached capacity.
    #[inline]
    fn buffer_full(&self) -> bool {
        self.buffer.len() == self.buffer_size
    }

    /// Send all bytes in the current buffer to the compressor.
    ///
    /// If `is_last` is `true`, the message sent to the compressor will also have the `is_last` true flag set
    /// and the compressor will finish the BGZF stream.
    ///
    /// If `is_last` is not true then only full block will be sent. If `is_last` is true, an incomplete block may be set
    /// as the final block.
    fn flush_bytes(&mut self, is_last: bool) -> std::io::Result<()> {
        if is_last || self.buffer_full() {
            self.send_block(is_last)?;
        }
        Ok(())
    }

    /// Send a single block
    fn send_block(&mut self, is_last: bool) -> std::io::Result<()> {
        let bytes = self.buffer.split_to(self.buffer.len()).freeze();
        let (mut m, r) = CompressorMessage::new_parts(self.writer_index, bytes);
        m.is_last = is_last;
        self.writer_tx
            .send(r)
            .map_err(|_e| io::Error::new(io::ErrorKind::Other, PoolError::ChannelSend))?;
        self.compressor_tx
            .send(m)
            .map_err(|_e_| io::Error::new(io::ErrorKind::Other, PoolError::ChannelSend))
    }

    /// Flush any remaining bytes and consume self, triggering drops of the senders.
    pub fn close(mut self) -> std::io::Result<()> {
        self.flush_bytes(true)
    }
}

impl Drop for PooledWriter {
    /// Drop [`PooledWriter`].
    ///
    /// This will flush the writer.
    fn drop(&mut self) {
        self.flush_bytes(true).unwrap();
    }
}

impl Write for PooledWriter {
    /// Send all bytes in `buf` to the [`Pool`].
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut bytes_added = 0;

        while bytes_added < buf.len() {
            let bytes_to_append =
                std::cmp::min(buf.len() - bytes_added, self.buffer_size - self.buffer.len());

            self.buffer.extend_from_slice(&buf[bytes_added..bytes_added + bytes_to_append]);
            bytes_added += bytes_to_append;
            if self.buffer_full() {
                self.send_block(false)?;
            }
        }

        Ok(buf.len())
    }

    /// Send whatever is in the current buffer even if it is not a full buffer.
    fn flush(&mut self) -> std::io::Result<()> {
        self.flush_bytes(false)
    }
}

////////////////////////////////////////////////////////////////////////////////
// The Compressor trait
////////////////////////////////////////////////////////////////////////////////

/// A [`Compressor`] is used in the compressor pool to compress bytes.
///
/// An implementation must be provided as a type to the [`Pool::new`] function so that the pool
/// knows what kind of compression to use.
///
/// See the module level example for more details.
pub trait Compressor: Sized + Send + 'static
where
    Self::CompressionLevel: Clone + Send + 'static,
    Self::Error: Error + Send + 'static,
{
    type Error;
    type CompressionLevel;

    /// The `BLOCK_SIZE` is used to set the buffer size of the [`PooledWriter`]s and should match the max
    /// size allowed by the block compression format being used.
    const BLOCK_SIZE: usize = 65280;

    /// Create a new compressor with the given compression level.
    fn new(compression_level: Self::CompressionLevel) -> Self;

    /// Returns the default compression level for the compressor.
    fn default_compression_level() -> Self::CompressionLevel;

    /// Create an instance of the compression level.
    ///
    /// The validity of the compression level should be checked here.
    fn new_compression_level(compression_level: u8) -> Result<Self::CompressionLevel, Self::Error>;

    /// Compress a set of bytes into the `output` vec. If `is_last` is true, and depending on the
    /// block compression format, an EOF block may be appended as well.
    fn compress(
        &mut self,
        input: &[u8],
        output: &mut Vec<u8>,
        is_last: bool,
    ) -> Result<(), Self::Error>;
}

////////////////////////////////////////////////////////////////////////////////
// The messages passed between threads
////////////////////////////////////////////////////////////////////////////////

/// A message that is sent from a [`PooledWriter`] to the compressor threadpool within a [`Pool`].
#[derive(Debug)]
struct CompressorMessage {
    /// The index of the destination writer
    writer_index: usize,
    /// The bytes to compress.
    buffer: Bytes,
    /// Where the compressed bytes will be sent after compression.
    oneshot: Sender<WriterMessage>,
    /// A sentinel value to let the compressor know that the BGZF stream needs an EOF.
    is_last: bool,
}

impl CompressorMessage {
    fn new_parts(writer_index: usize, buffer: Bytes) -> (Self, Receiver<WriterMessage>) {
        let (tx, rx) = flume::unbounded(); // oneshot channel
        let new = Self { writer_index, buffer, oneshot: tx, is_last: false };
        (new, rx)
    }
}

/// The compressed bytes to be written to a file.
///
/// This is sent from the compressor threadpool to the writer queue in the writer threadpool
/// via the one-shot channel provided by the [`PooledWriter`].
#[derive(Debug)]
struct WriterMessage {
    buffer: Vec<u8>,
}

////////////////////////////////////////////////////////////////////////////////
// The PoolBuilder struct and impls
////////////////////////////////////////////////////////////////////////////////

/// A struct to make building up a Pool simpler.  The builder should be constructed using
/// [`PoolBuilder::new`], which provides the user control over the sizes of the queues used for
/// compression and writing.  It should be noted that a single compression queue is created,
/// and one writer queue per writer exchanged.  A good starting point for these queue sizes is
/// two times the number of threads.
///
/// Once created various functions can configure aspects of the pool.  It is best practice, though
/// not required, to configure the builder _before_ exchanging writers.
///
/// Once the builder is configured writers may be exchanged for [`PooledWriter`]s using the
/// [`PoolBuilder::exchange`] function, which consumes the provided writer and returns a new
/// writer that can be used in it's place.
///
/// After exchanging all writers the pool may be created and started with [`PoolBuilder::build`]
/// which consumes the builder and after which no more writers may be exchanged.
pub struct PoolBuilder<W, C>
where
    W: Write + Send + 'static,
    C: Compressor,
{
    writer_index: usize,
    compression_level: C::CompressionLevel,
    queue_size: usize,
    threads: usize,
    compressor_tx: Sender<CompressorMessage>,
    compressor_rx: Receiver<CompressorMessage>,
    writers: Vec<W>,
    writer_txs: Vec<Sender<Receiver<WriterMessage>>>,
    writer_rxs: Vec<Receiver<Receiver<WriterMessage>>>,
}

impl<W, C> PoolBuilder<W, C>
where
    W: Write + Send + 'static,
    C: Compressor,
{
    /// Creates a new PoolBuilder that can be used to configure and build a [`Pool`].
    /// The `queue_size` must be greater than the number of `threads`.
    pub fn new(queue_size: usize, threads: usize) -> Self {
        assert!(threads > 0, "Cannot construct a pooled writer with 0 threads");
        assert!(
            queue_size > threads,
            "Queue size ({}) must be > threads ({}).",
            queue_size,
            threads
        );

        let (compressor_tx, compressor_rx) = flume::bounded(queue_size);

        PoolBuilder {
            writer_index: 0,
            compression_level: C::default_compression_level(),
            queue_size,
            threads,
            compressor_tx,
            compressor_rx,
            writers: vec![],
            writer_txs: vec![],
            writer_rxs: vec![],
        }
    }

    /// Sets the compression level that will be used by the [[Pool]].
    pub fn compression_level(mut self, level: u8) -> PoolResult<Self> {
        self.compression_level = C::new_compression_level(level)
            .map_err(|e| PoolError::CompressionError(e.to_string()))?;
        Ok(self)
    }

    /// Exchanges a writer for a [[PooledWriter]].
    pub fn exchange(&mut self, writer: W) -> PooledWriter {
        let (tx, rx): (Sender<Receiver<WriterMessage>>, Receiver<Receiver<WriterMessage>>) =
            flume::bounded(self.queue_size);
        let p = PooledWriter::new::<C>(self.writer_index, self.compressor_tx.clone(), tx.clone());

        self.writer_index += 1;
        self.writers.push(writer);
        self.writer_txs.push(tx);
        self.writer_rxs.push(rx);
        p
    }

    /// Consumes the builder and generates the [[Pool]] ready for use.
    pub fn build(self) -> PoolResult<Pool> {
        // Create the channel to gracefully signal a shutdown of the pool
        let (shutdown_tx, shutdown_rx) = flume::unbounded();

        // Start the pool manager thread and thread pools
        let handle = std::thread::spawn(move || {
            Pool::pool_main::<W, C>(
                self.threads,
                self.compression_level,
                self.compressor_rx,
                self.writer_rxs,
                self.writers,
                shutdown_rx,
            )
        });

        let mut pool = Pool {
            compressor_tx: Some(self.compressor_tx),
            writers_txs: Some(self.writer_txs),
            shutdown_tx: Some(shutdown_tx),
            pool_handle: Some(handle),
        };

        Ok(pool)
    }
}

////////////////////////////////////////////////////////////////////////////////
// The Pool struct and impls
////////////////////////////////////////////////////////////////////////////////

/// A [`Pool`] orchestrates two different threadpools, a compressor pool and a writer pool.
///
/// The pool is suitable for scenarios where there are many more writers than threads, efficiently
/// managing resources for M writers to N threads.
#[derive(Debug)]
pub struct Pool {
    /// The join handle for the thread that manages all pool resources and coordination.
    pool_handle: Option<JoinHandle<PoolResult<()>>>,
    /// The send end of the channel for communicating with the compressor pool.
    compressor_tx: Option<Sender<CompressorMessage>>,
    /// The send halves of the channels for the [`PooledWriter`]s to enqueue the one-shot channels.
    writers_txs: Option<Vec<Sender<Receiver<WriterMessage>>>>,
    /// Sentinel channel to tell the pool management thread to shutdown.
    shutdown_tx: Option<Sender<()>>,
}

impl Pool {
    /// The main "run" method for the pool that orchestrates all the pieces.
    ///
    /// The [`PooledWriter`]s are sending to the compressor, the compressor compresses them, then forwards the compressed bytes.
    /// The bytes are forwarded to a queue per writer and the writer threads are iterating over that queue pulling down
    /// all values in the queue at once and writing till the queue is empty.
    ///
    /// # Arguments
    /// - `num_threads` - The number of threads to use.
    /// - `compression_level` - The compression level to use for the [`Compressor`] pool.
    /// - `compressor_rx ` - The receiving end of the channel for communicating with the compressor pool.
    /// - `writer_rxs ` - The receive halves of the channels for the [`PooledWriter`]s to enqueue the one-shot channels.
    /// - `writers` - The writers that were exchanged for [`PooledWriter`]s.
    /// - `shutdown_rx` - Sentinel channel to tell the pool management thread to shutdown.
    #[allow(clippy::unnecessary_wraps, clippy::needless_collect, clippy::needless_pass_by_value)]
    fn pool_main<W, C>(
        num_threads: usize,
        compression_level: C::CompressionLevel,
        compressor_rx: Receiver<CompressorMessage>,
        writer_rxs: Vec<Receiver<Receiver<WriterMessage>>>, // must be pass by value to allow for easy sharing between threads
        writers: Vec<W>,
        shutdown_rx: Receiver<()>,
    ) -> PoolResult<()>
    where
        W: Write + Send + 'static,
        C: Compressor,
    {
        // Add locks to the writers
        let writers: Arc<Vec<_>> =
            Arc::new(writers.into_iter().map(|w| Arc::new(Mutex::new(w))).collect());

        // Generate one more channel for queuing up information about when a writer has data
        // available to be written
        let (write_available_tx, write_available_rx): (Sender<usize>, Receiver<usize>) =
            flume::unbounded();

        let thread_handles: Vec<JoinHandle<PoolResult<()>>> = (0..num_threads)
            .map(|thread_idx| {
                let compressor_rx = compressor_rx.clone();
                let mut compressor = C::new(compression_level.clone());
                let writer_rxs = writer_rxs.clone();
                let writers = writers.clone();
                let shutdown_rx = shutdown_rx.clone();
                let sleep_delay = Duration::from_millis(25);
                let write_available_tx = write_available_tx.clone();
                let write_available_rx = write_available_rx.clone();

                std::thread::spawn(move || {
                    loop {
                        let mut did_something = false;

                        // Try to process one compression message
                        if let Ok(message) = compressor_rx.try_recv() {
                            // Compress the buffer in the message
                            let chunk = &message.buffer;
                            // Compress will correctly resize the compressed vec.
                            let mut compressed = Vec::new();
                            compressor
                                .compress(chunk, &mut compressed, message.is_last)
                                .map_err(|e| PoolError::CompressionError(e.to_string()))?;
                            message
                                .oneshot
                                .send(WriterMessage { buffer: compressed })
                                .map_err(|_e| PoolError::ChannelSend);
                            write_available_tx.send(message.writer_index);
                            did_something = true;
                        }

                        // Then try to process one write message
                        if let Ok(writer_index) = write_available_rx.try_recv() {
                            let mut writer = writers[writer_index].lock();
                            let writer_rx = &writer_rxs[writer_index];
                            let one_shot_rx = writer_rx.recv()?;
                            let write_message = one_shot_rx.recv()?;
                            writer.write_all(&write_message.buffer)?;
                            did_something = true;
                        }

                        // If we didn't do anything either sleep for a few ms to avoid busy-waiting
                        // or if shutdown is requested and all the channels are empty, terminate.
                        if !did_something {
                            if shutdown_rx.is_disconnected()
                                && write_available_rx.is_empty()
                                && compressor_rx.is_empty()
                                && writer_rxs.iter().all(|w| w.is_empty())
                            {
                                break;
                            } else {
                                std::thread::sleep(sleep_delay);
                            }
                        }
                    }

                    Ok(())
                })
            })
            .collect();

        // Close writer handles
        thread_handles.into_iter().try_for_each(|handle| match handle.join() {
            Ok(result) => result,
            Err(e) => std::panic::resume_unwind(e),
        });

        // Flush each writer
        writers.iter().try_for_each(|w| w.lock().flush())?;

        Ok(())
    }

    /// Shutdown all pool resources and close all channels.
    ///
    /// Ideally the [`PooledWriter`]s should all have been flushed first, that is up to the user. Any
    /// further attempts to send to the [`Pool`] will return an error.
    pub fn stop_pool(&mut self) -> Result<(), PoolError> {
        let compressor_queue = self.compressor_tx.take().unwrap();
        while !compressor_queue.is_empty() {
            // Wait for compression to finish before dropping the sender
        }
        drop(compressor_queue);

        // Shutdown called to force writers to start checking their receivers for disconnection / empty
        drop(self.shutdown_tx.take());

        // Drop the copy of the writer senders that the pool holds
        // TODO: the pool probably doesn't need these anyways.
        self.writers_txs.take().into_iter().enumerate().for_each(|(i, w)| {
            drop(w);
        });
        // Wait on the pool thread to finish and pull any errors from it
        match self.pool_handle.take().unwrap().join() {
            Ok(result) => result,
            Err(e) => std::panic::resume_unwind(e),
        }
    }
}

impl Drop for Pool {
    fn drop(&mut self) {
        // Check if `stop_pool` has already been called. If it hasn't, call it.
        if self.compressor_tx.is_some() && self.pool_handle.is_some() && self.writers_txs.is_some()
        {
            self.stop_pool().unwrap();
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// Tests
////////////////////////////////////////////////////////////////////////////////

#[cfg(test)]
mod test {
    use std::{
        assert_eq, format,
        fs::File,
        io::{BufReader, BufWriter},
        path::{Path, PathBuf},
        vec,
    };

    use crate::bgzf::BgzfCompressor;

    use super::*;
    use ::bgzf::Reader;
    use proptest::prelude::*;
    use tempfile::tempdir;

    fn create_output_writer<P: AsRef<Path>>(path: P) -> BufWriter<File> {
        BufWriter::new(File::create(path).unwrap())
    }

    fn create_output_file_name(name: impl AsRef<Path>, dir: impl AsRef<Path>) -> PathBuf {
        let path = dir.as_ref().to_path_buf();
        path.join(name)
    }

    #[test]
    fn test_simple() {
        let dir = tempdir().unwrap();
        let output_names: Vec<PathBuf> = (0..20)
            .into_iter()
            .map(|i| create_output_file_name(format!("test.{}.txt.gz", i), &dir.path()))
            .collect();

        let output_writers: Vec<BufWriter<File>> =
            output_names.iter().map(create_output_writer).collect();
        let mut builder =
            PoolBuilder::<_, BgzfCompressor>::new(20, 8).compression_level(2).unwrap();
        let mut pooled_writers: Vec<PooledWriter> =
            output_writers.into_iter().map(|w| builder.exchange(w)).collect();
        let mut pool = builder.build().unwrap();

        for (i, writer) in pooled_writers.iter_mut().enumerate() {
            writer.write_all(format!("This is writer {}.", i).as_bytes()).unwrap();
        }
        pooled_writers.into_iter().try_for_each(|mut w| w.flush()).unwrap();
        pool.stop_pool();

        for (i, path) in output_names.iter().enumerate() {
            let mut reader = Reader::new(BufReader::new(File::open(path).unwrap()));
            let mut actual = vec![];
            reader.read_to_end(&mut actual).unwrap();
            assert_eq!(actual, format!("This is writer {}.", i).as_bytes());
        }
    }

    proptest! {
        // This test takes around 20 minutes on a 32 core machine to run but is very comprehensive.
        // Run with `cargo test -- --ignored`
        #[ignore]
        #[test]
        fn test_complete(
            input_size in 1..=BUFSIZE * 4,
            buf_size in 1..=BUFSIZE,
            num_output_files in 1..2*num_cpus::get(),
            threads in 1..=2+num_cpus::get(),
            comp_level in 1..=8_u8,
            write_size in 1..=2*BUFSIZE,
        ) {
            let dir = tempdir().unwrap();
            let output_names: Vec<PathBuf> = (0..num_output_files)
                .into_iter()
                .map(|i| create_output_file_name(format!("test.{}.txt.gz", i), &dir.path()))
                .collect();
            let output_writers: Vec<_> = output_names.iter().map(create_output_writer).collect();

            let mut builder = PoolBuilder::<_, BgzfCompressor>::new(threads * 2, threads)
                .compression_level(comp_level)?;

            let mut pooled_writers: Vec<_> = output_writers.into_iter().map(|w| builder.exchange(w)).collect();
            let mut pool = builder.build()?;

            let inputs: Vec<Vec<u8>> = (0..num_output_files).map(|_| {
                (0..input_size).map(|_| rand::random::<u8>()).collect()
            }).collect();

            let chunks = (input_size as f64 / write_size as f64).ceil() as usize;

            // write a chunk to each writer (could randomly select the writers?)
            for i in (0..chunks) {
                for (j, writer) in pooled_writers.iter_mut().enumerate() {
                    let input = &inputs[j];
                    let bytes = &input[write_size * i..std::cmp::min(write_size * (i + 1), input.len())];
                    writer.write_all(bytes).unwrap()
                }
            }

            pooled_writers.into_iter().try_for_each(|mut w| w.flush()).unwrap();
            pool.stop_pool();

            for (i, path) in output_names.iter().enumerate() {
                let mut reader = Reader::new(BufReader::new(File::open(path).unwrap()));
                let mut actual = vec![];
                reader.read_to_end(&mut actual).unwrap();
                assert_eq!(actual, inputs[i]);
            }

        }
    }
}
