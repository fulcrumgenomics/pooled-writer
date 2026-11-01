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
//! The [`Pool`] consists of a single thread pool that consumes work from both a compression queue
//! and a writing queue.  All concurrency is managed via message passing over channels.
//!
//! Every time the internal buffer of a [`PooledWriter`] reaches capacity (defined by
//! [`Compressor::BLOCK_SIZE`]) it sends two messages:
//! 1. It sends a message over the corresponding writer's channel to the writer pool, enqueueing
//!    a one-shot receiver channel in the writers queue that will receive the compressed bytes
//!    once the compressor is done. This is done to maintain the output order.
//! 2. It sends a message to the compressor pool that contains a buffer of bytes to compress
//!    as well as the sender side of the one-shot channel to send the compressed bytes on.
//!
//! The threads in the thread pool loop continuously until the pool is shut down, and attempt
//! first receive and compress one block, then secondly to receive and write one compressed block.
//! A third internal channel is used to manage the queue of writes to be performed so that the
//! individual per-writer channels (of which there may be many) are only polled if there is likely
//! to be data available for writing.  When data is available to be written, the appropriate
//! underlying writer is locked, and the data written.
//!
//! When all writing is complete, close every [`PooledWriter`] with [`PooledWriter::close`] (or
//! drop it, which does the same but ignores errors), then stop the pool with
//! [`Pool::stop_pool`].  Stopping the pool waits for all data sent to it to be compressed and
//! written, and is the only way to learn whether every output is complete:
//! [`PooledWriter::close`] returns once the writer's final block is queued, not written.  A
//! writer still open when the pool stops loses the data it has buffered, so [`Pool::stop_pool`]
//! returns [`PoolError::WriterNotClosed`], and later attempts to send data from that writer fail.
//!
//! If compressing or writing any block fails, or a thread in the pool panics, the pool stops.
//! From then on any [`PooledWriter`] that sends a block to the pool, or is flushed or closed,
//! returns an error with the same kind and message, and [`Pool::stop_pool`] returns the original
//! error, or panics if a thread panicked.  Errors name the writer they concern by its
//! [`PooledWriter::writer_index`].
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
//!     let mut builder = PoolBuilder::<_, BgzfCompressor>::new()
//!         .threads(8)
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
    clippy::missing_panics_doc,
    clippy::missing_errors_doc,
    clippy::must_use_candidate,
    clippy::module_name_repetitions
)]

#[cfg(feature = "bgzf_compressor")]
pub mod bgzf;

use std::time::Duration;
use std::{
    any::Any,
    error::Error,
    io::{self, Write},
    panic::{self, AssertUnwindSafe},
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
};

use bytes::{Bytes, BytesMut};
use flume::{self, Receiver, Sender, bounded};
use parking_lot::Mutex;
use thiserror::Error;

/// Convenience type for functions that return [`PoolError`].
type PoolResult<T> = Result<T, PoolError>;

/// Represents errors that may be generated by any `Pool` related functionality.
///
/// Errors that concern one writer name it by its [`PooledWriter::writer_index`].
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
    /// Writing to or flushing a writer failed.
    #[error("Failed to write to writer {writer_index}: {source}")]
    WriterIo { writer_index: usize, source: io::Error },
    /// Compressing a block for a writer failed.
    #[error("Error compressing data for writer {writer_index}: {message}")]
    WriterCompression { writer_index: usize, message: String },
    /// A writer was still open when the pool stopped, so the data it had buffered was lost.
    #[error("Writer {writer_index} was not closed before the pool was stopped")]
    WriterNotClosed { writer_index: usize },
}

impl PoolError {
    /// Returns an error with the same variant, kind and message, for reporting a failure again
    /// after the original has been returned.  [`io::Error`] isn't `Clone`, so an underlying I/O
    /// error is rebuilt from its kind and message.
    fn duplicate(&self) -> Self {
        let copy_io_error = |error: &io::Error| io::Error::new(error.kind(), error.to_string());
        match self {
            Self::ChannelSend => Self::ChannelSend,
            Self::ChannelReceive(error) => Self::ChannelReceive(*error),
            Self::CompressionError(message) => Self::CompressionError(message.clone()),
            Self::Io(error) => Self::Io(copy_io_error(error)),
            Self::WriterIo { writer_index, source } => {
                Self::WriterIo { writer_index: *writer_index, source: copy_io_error(source) }
            }
            Self::WriterCompression { writer_index, message } => {
                Self::WriterCompression { writer_index: *writer_index, message: message.clone() }
            }
            Self::WriterNotClosed { writer_index } => {
                Self::WriterNotClosed { writer_index: *writer_index }
            }
        }
    }

    /// Returns the kind of the underlying I/O error, or [`io::ErrorKind::Other`] if there is none.
    fn io_error_kind(&self) -> io::ErrorKind {
        match self {
            Self::Io(error) | Self::WriterIo { source: error, .. } => error.kind(),
            _ => io::ErrorKind::Other,
        }
    }
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
    writer_tx: Sender<oneshot::Receiver<WriterMessage>>,
    /// The internal buffer to gather bytes to send.
    buffer: BytesMut,
    /// The desired size of the internal buffer.
    buffer_size: usize,
    /// The first failure of the pool, if any, shared by all its writers.
    pool_failure: Arc<PoolFailure>,
    /// Whether [`PooledWriter::close`] has been called, after which drop must not send another
    /// final block.
    closed: bool,
}

impl PooledWriter {
    /// Create a new [`PooledWriter`] that has an internal buffer capacity that matches [`bgzf::BGZF_BLOCK_SIZE`].
    ///
    /// # Arguments
    /// - `index` - a usize representing that this is the nth pooled writer created within the pool
    /// - `compressor_tx` - The channel to send uncompressed bytes to the compressor pool.
    /// - `writer_tx` - The `Send` end of the channel that transmits the `Receiver` end of the one-shot
    ///                 channel, which will be consumed when the compressor sends the compressed bytes.
    /// - `pool_failure` - The pool's record of its first failure.
    fn new<C>(
        index: usize,
        compressor_tx: Sender<CompressorMessage>,
        writer_tx: Sender<oneshot::Receiver<WriterMessage>>,
        pool_failure: Arc<PoolFailure>,
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
            pool_failure,
            closed: false,
        }
    }

    /// Returns the index that identifies this writer in [`PoolError`]s: the number of writers
    /// exchanged with the [`PoolBuilder`] before it, so the first writer exchanged is 0.
    pub fn writer_index(&self) -> usize {
        self.writer_index
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

    /// Send a single block.
    ///
    /// Fails without sending if the pool has already failed, because its output is incomplete.
    fn send_block(&mut self, is_last: bool) -> std::io::Result<()> {
        if let Some(error) = self.pool_failure.to_io_error() {
            return Err(error);
        }
        let bytes = self.buffer.split_to(self.buffer.len()).freeze();
        let (mut m, r) = CompressorMessage::new_parts(self.writer_index, bytes);
        m.is_last = is_last;
        self.writer_tx.send(r).map_err(|_e| self.disconnected_error())?;
        self.compressor_tx.send(m).map_err(|_e| self.disconnected_error())
    }

    /// The error for a send that failed because the pool's threads have exited: the pool's
    /// failure if it had one, otherwise [`PoolError::ChannelSend`] as the pool was stopped.
    fn disconnected_error(&self) -> io::Error {
        self.pool_failure
            .to_io_error()
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::Other, PoolError::ChannelSend))
    }

    /// Flush any remaining bytes and consume self, triggering drops of the senders.
    ///
    /// Returns once the final block is queued, so `Ok` doesn't mean the output is complete: only
    /// [`Pool::stop_pool`] reports that.  Fails if the pool has already been stopped, or if it has
    /// failed, in which case the error has the kind and message of the pool's first failure.
    pub fn close(mut self) -> std::io::Result<()> {
        // Set before sending so that drop never sends a second final block, even if this fails.
        self.closed = true;
        self.flush_bytes(true)
    }
}

impl Drop for PooledWriter {
    /// Drop [`PooledWriter`].
    ///
    /// If the writer was not closed this sends any buffered bytes and the final block, ignoring
    /// errors; use [`PooledWriter::close`] to learn whether that succeeded.
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.flush_bytes(true);
        }
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

    /// Fails if the pool has failed, and otherwise sends the buffer if it is full.
    ///
    /// A partial block stays buffered until more bytes are written or the writer is closed, so
    /// that flushing doesn't produce small blocks.  The underlying writer is flushed when the pool
    /// stops.
    fn flush(&mut self) -> std::io::Result<()> {
        if let Some(error) = self.pool_failure.to_io_error() {
            return Err(error);
        }
        self.flush_bytes(false)
    }
}

////////////////////////////////////////////////////////////////////////////////
// The Compressor trait
////////////////////////////////////////////////////////////////////////////////

/// A [`Compressor`] is used in the compressor pool to compress bytes.
///
/// An implementation must be provided as a type to the [`PoolBuilder`] so that the pool knows what
/// kind of compression to use.
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
struct CompressorMessage {
    /// The index of the destination writer
    writer_index: usize,
    /// The bytes to compress.
    buffer: Bytes,
    /// Where the compressed bytes will be sent after compression.
    oneshot_tx: oneshot::Sender<WriterMessage>,
    /// A sentinel value to let the compressor know that the BGZF stream needs an EOF.
    is_last: bool,
}

impl CompressorMessage {
    fn new_parts(writer_index: usize, buffer: Bytes) -> (Self, oneshot::Receiver<WriterMessage>) {
        let (tx, rx) = oneshot::channel();
        let new = Self { writer_index, buffer, oneshot_tx: tx, is_last: false };
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
    /// Whether this is the writer's final block.
    is_last: bool,
}

/// Internal enum used by worker threads to dispatch between compression and write work.
enum WorkItem {
    Compress(CompressorMessage),
    Write(usize),
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
/// not required, to configure the builder _before_ exchanging writers.  The exception is
/// `queue_size` that may _not_ be set after any writers have been exchanged.  If not set manually
/// then `queue_size` defaults to the number of threads multiplied by
/// [`PoolBuilder::QUEUE_SIZE_THREAD_MULTIPLES`].
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
    queue_size: Option<usize>,
    threads: usize,
    compressor_tx: Option<Sender<CompressorMessage>>,
    compressor_rx: Option<Receiver<CompressorMessage>>,
    writers: Vec<W>,
    writer_txs: Vec<Sender<oneshot::Receiver<WriterMessage>>>,
    writer_rxs: Vec<Receiver<oneshot::Receiver<WriterMessage>>>,
    pool_failure: Arc<PoolFailure>,
}

impl<W, C> PoolBuilder<W, C>
where
    W: Write + Send + 'static,
    C: Compressor,
{
    /// By default queue sizes will be set to threads * this constant.
    pub const QUEUE_SIZE_THREAD_MULTIPLES: usize = 50;

    /// The default number of threads that will be used if not otherwise configured
    pub const DEFAULT_THREADS: usize = 4;

    /// Creates a new PoolBuilder that can be used to configure and build a [`Pool`].
    pub fn new() -> Self {
        PoolBuilder {
            writer_index: 0,
            compression_level: C::default_compression_level(),
            queue_size: None,
            threads: Self::DEFAULT_THREADS,
            compressor_tx: None,
            compressor_rx: None,
            writers: vec![],
            writer_txs: vec![],
            writer_rxs: vec![],
            pool_failure: Arc::default(),
        }
    }

    /// Sets the number of threads that will be used by the [[Pool]].
    ///
    /// Will panic if set to 0.
    pub fn threads(mut self, threads: usize) -> Self {
        assert!(threads > 0, "Must provide a number of threads greater than 0.");
        self.threads = threads;
        self
    }

    /// Sets the size of queues used by the pool [[Pool]].  The same size is used for
    /// a) the queue of byte buffers to be compressed, b) the per-sample queues to receive
    /// compressed bytes, and c) a control queue to manage writing to the underlying writers.
    ///
    /// In the worst case scenario the pool can be holding both queue_size uncompressed blocks
    /// _and_ queue_size compressed blocks in memory when it cannot keep up with the incoming
    /// load of writes.
    ///
    ///
    ///
    /// Will panic if called _after_ writers have been created because queues will already have
    /// been created.
    pub fn queue_size(mut self, queue_size: usize) -> Self {
        assert!(self.writers.is_empty(), "Cannot set queue_size after writers are exchanged.");
        self.queue_size = Some(queue_size);
        self
    }

    /// Sets the compression level that will be used by the [[Pool]].
    pub fn compression_level(mut self, level: u8) -> PoolResult<Self> {
        self.compression_level = C::new_compression_level(level)
            .map_err(|e| PoolError::CompressionError(e.to_string()))?;
        Ok(self)
    }

    /// If queues/channels are not yet setup, initialize them.
    fn ensure_queue_is_setup(&mut self) {
        if self.compressor_tx.is_none() && self.compressor_rx.is_none() {
            if self.queue_size.is_none() {
                self.queue_size = Some(self.threads * Self::QUEUE_SIZE_THREAD_MULTIPLES);
            }

            let (tx, rx) = bounded(self.queue_size.unwrap());
            self.compressor_tx = Some(tx);
            self.compressor_rx = Some(rx);
        }
    }

    /// Exchanges a writer for a [[PooledWriter]].
    pub fn exchange(&mut self, writer: W) -> PooledWriter {
        // Make sure queue/channel configuration is done
        self.ensure_queue_is_setup();

        let (tx, rx): (
            Sender<oneshot::Receiver<WriterMessage>>,
            Receiver<oneshot::Receiver<WriterMessage>>,
        ) = flume::bounded(self.queue_size.expect("Unreachable"));

        let p = PooledWriter::new::<C>(
            self.writer_index,
            self.compressor_tx.as_ref().expect("Unreachable").clone(),
            tx.clone(),
            self.pool_failure.clone(),
        );

        self.writer_index += 1;
        self.writers.push(writer);
        self.writer_txs.push(tx);
        self.writer_rxs.push(rx);
        p
    }

    /// Consumes the builder and generates the [[Pool]] ready for use.
    pub fn build(mut self) -> PoolResult<Pool> {
        // Make sure the queue/channel configuration is done - this could be necessary if
        // a pool is created by zero writers exchanged.
        self.ensure_queue_is_setup();

        // Create the channel to gracefully signal a shutdown of the pool
        let (shutdown_tx, shutdown_rx) = flume::unbounded();

        let pool_failure = self.pool_failure.clone();

        // Start the pool manager thread and thread pools
        let handle = std::thread::spawn(move || {
            Pool::pool_main::<W, C>(
                self.threads,
                self.compression_level,
                self.compressor_rx.expect("Unreachable."),
                self.writer_rxs,
                self.writers,
                shutdown_rx,
                self.pool_failure,
            )
        });

        let pool = Pool {
            compressor_tx: self.compressor_tx,
            shutdown_tx: Some(shutdown_tx),
            pool_handle: Some(handle),
            pool_failure,
            panic_message: None,
        };

        Ok(pool)
    }
}

impl<W, C> Default for PoolBuilder<W, C>
where
    W: Write + Send + 'static,
    C: Compressor,
{
    fn default() -> Self {
        Self::new()
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
    /// Sentinel channel to tell the pool management thread to shutdown.
    shutdown_tx: Option<Sender<()>>,
    /// The pool's first failure, which repeated calls to [`Pool::stop_pool`] return again.
    pool_failure: Arc<PoolFailure>,
    /// The message of the panic that stopped the pool, if one did, so that repeated calls to
    /// [`Pool::stop_pool`] panic again.
    panic_message: Option<String>,
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
    /// - `pool_failure` - The record of the pool's first failure, shared with the [`PooledWriter`]s.
    ///
    /// Returns the first error raised by any thread, or [`PoolError::WriterNotClosed`] if a
    /// writer's final block was never written, and panics if any thread panicked.
    #[allow(clippy::needless_collect, clippy::needless_pass_by_value)]
    fn pool_main<W, C>(
        num_threads: usize,
        compression_level: C::CompressionLevel,
        compressor_rx: Receiver<CompressorMessage>,
        writer_rxs: Vec<Receiver<oneshot::Receiver<WriterMessage>>>, // must be pass by value to allow for easy sharing between threads
        writers: Vec<W>,
        shutdown_rx: Receiver<()>,
        pool_failure: Arc<PoolFailure>,
    ) -> PoolResult<()>
    where
        W: Write + Send + 'static,
        C: Compressor,
    {
        // Add locks to the writers
        let writers: Arc<Vec<_>> =
            Arc::new(writers.into_iter().map(|w| Arc::new(Mutex::new(w))).collect());

        // Whether each writer's final block has been written, i.e. whether it was closed.
        let final_block_written: Arc<Vec<AtomicBool>> =
            Arc::new(writers.iter().map(|_| AtomicBool::new(false)).collect());

        // Generate one more channel for queuing up information about when a writer has data
        // available to be written
        let (write_available_tx, write_available_rx): (Sender<usize>, Receiver<usize>) =
            flume::unbounded();

        let thread_handles: Vec<JoinHandle<()>> = (0..num_threads)
            .map(|_| {
                let compressor_rx = compressor_rx.clone();
                let mut compressor = C::new(compression_level.clone());
                let writer_rxs = writer_rxs.clone();
                let writers = writers.clone();
                let final_block_written = final_block_written.clone();
                let shutdown_rx = shutdown_rx.clone();
                let write_available_tx = write_available_tx.clone();
                let write_available_rx = write_available_rx.clone();
                let pool_failure = pool_failure.clone();
                let select_timeout = Duration::from_millis(100);

                let worker = {
                    let pool_failure = pool_failure.clone();
                    move || -> PoolResult<()> {
                        // Reuse a single compression buffer per thread to avoid
                        // re-allocating ~70KB on every block.
                        let mut compress_buf = Vec::new();

                        loop {
                            // Stop as soon as any thread fails, as the output is already
                            // incomplete.  Once every thread has stopped, pool_main returns and
                            // drops its receivers, so PooledWriters blocked on a full queue fail
                            // instead of hanging.
                            if pool_failure.has_failed() {
                                break;
                            }

                            // Try non-blocking receives first (fast path under load).
                            // Then fall through to Selector which blocks until work
                            // arrives, avoiding the old sleep(25ms) polling delay.
                            let item = if let Ok(msg) = compressor_rx.try_recv() {
                                Some(WorkItem::Compress(msg))
                            } else if let Ok(idx) = write_available_rx.try_recv() {
                                Some(WorkItem::Write(idx))
                            } else {
                                flume::Selector::new()
                                    .recv(&compressor_rx, |r| r.ok().map(WorkItem::Compress))
                                    .recv(&write_available_rx, |r| r.ok().map(WorkItem::Write))
                                    .wait_timeout(select_timeout)
                                    .ok()
                                    .flatten()
                            };

                            match item {
                                Some(WorkItem::Compress(message)) => {
                                    compress_buf.clear();
                                    // Errors and panics are recorded before `message` is dropped,
                                    // because dropping its one-shot sender makes a thread waiting
                                    // to write the block fail, and that error mustn't be recorded
                                    // first.
                                    let compress = || {
                                        compressor.compress(
                                            &message.buffer,
                                            &mut compress_buf,
                                            message.is_last,
                                        )
                                    };
                                    let compressed =
                                        match panic::catch_unwind(AssertUnwindSafe(compress)) {
                                            Ok(compressed) => compressed,
                                            Err(payload) => {
                                                pool_failure.record_panic(&*payload);
                                                panic::resume_unwind(payload);
                                            }
                                        };
                                    if let Err(e) = compressed {
                                        pool_failure.record(PoolError::WriterCompression {
                                            writer_index: message.writer_index,
                                            message: e.to_string(),
                                        });
                                        break;
                                    }
                                    message
                                        .oneshot_tx
                                        .send(WriterMessage {
                                            buffer: compress_buf.clone(),
                                            is_last: message.is_last,
                                        })
                                        .map_err(|_e| PoolError::ChannelSend)?;
                                    write_available_tx
                                        .send(message.writer_index)
                                        .map_err(|_e| PoolError::ChannelSend)?;
                                }
                                Some(WorkItem::Write(writer_index)) => {
                                    let mut writer = writers[writer_index].lock();
                                    let writer_rx = &writer_rxs[writer_index];
                                    let one_shot_rx = writer_rx.recv()?;
                                    let write_message =
                                        one_shot_rx.recv().map_err(|_| PoolError::ChannelSend)?;
                                    writer.write_all(&write_message.buffer).map_err(|source| {
                                        PoolError::WriterIo { writer_index, source }
                                    })?;
                                    if write_message.is_last {
                                        final_block_written[writer_index]
                                            .store(true, Ordering::Relaxed);
                                    }
                                }
                                None => {
                                    // Timeout or channel disconnect. Check if all work
                                    // is drained and shutdown was requested.
                                    if shutdown_rx.is_disconnected()
                                        && write_available_rx.is_empty()
                                        && compressor_rx.is_empty()
                                        && writer_rxs.iter().all(|w| w.is_empty())
                                    {
                                        break;
                                    }
                                }
                            }
                        }

                        Ok(())
                    }
                };

                // A panic is recorded as a failure, so that the other threads stop instead of
                // waiting for blocks the panicking thread will never send, and then resumed so
                // that it reaches whoever stops the pool.
                std::thread::spawn(move || match panic::catch_unwind(AssertUnwindSafe(worker)) {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => pool_failure.record(error),
                    Err(payload) => {
                        pool_failure.record_panic(&*payload);
                        panic::resume_unwind(payload);
                    }
                })
            })
            .collect();

        // Join every thread, even after a failure, so that none is left running.
        let mut first_panic = None;
        for handle in thread_handles {
            if let Err(payload) = handle.join() {
                first_panic.get_or_insert(payload);
            }
        }

        // Flush every writer, even after a failure, so that every block written reaches its
        // output.
        for (writer_index, writer) in writers.iter().enumerate() {
            if let Err(source) = writer.lock().flush() {
                pool_failure.record(PoolError::WriterIo { writer_index, source });
            }
        }

        // A writer whose final block wasn't written was still open, and its buffered data lost.
        let open_writer = final_block_written.iter().position(|w| !w.load(Ordering::Relaxed));
        if let Some(writer_index) = open_writer {
            pool_failure.record(PoolError::WriterNotClosed { writer_index });
        }

        if let Some(payload) = first_panic {
            panic::resume_unwind(payload);
        }
        pool_failure.take_error().map_or(Ok(()), Err)
    }

    /// Shutdown all pool resources and close all channels.
    ///
    /// The [`PooledWriter`]s should all be closed first.  Any further attempts to send to the
    /// [`Pool`] will return an error.
    ///
    /// Waits for all data sent to the pool to be compressed and written, then returns the first
    /// error raised while compressing, writing or flushing, with any underlying [`io::Error`]
    /// intact, or [`PoolError::WriterNotClosed`] if a writer was still open.  Panics if any of the
    /// pool's threads panicked.
    ///
    /// Calling this again has the same outcome: `Ok(())`, an error with the same variant, kind
    /// and message, or a panic with the same message.
    pub fn stop_pool(&mut self) -> Result<(), PoolError> {
        match self.shutdown() {
            Some(Ok(result)) => result,
            Some(Err(payload)) => panic::resume_unwind(payload),
            None => match &self.panic_message {
                Some(message) => panic!("{message}"),
                None => self.pool_failure.to_pool_error().map_or(Ok(()), Err),
            },
        }
    }

    /// Disconnects the pool's channels and waits for its threads to finish.
    ///
    /// Returns the pool's result, or the payload of a panic in one of its threads, or `None` if
    /// the pool has already been shut down.
    fn shutdown(&mut self) -> Option<std::thread::Result<PoolResult<()>>> {
        // Drop the compressor sender to disconnect the channel.  Buffered
        // messages are preserved by flume and will be drained by the worker
        // threads before they observe the disconnect and shut down.
        drop(self.compressor_tx.take());

        // Shutdown called to force writers to start checking their receivers for disconnection / empty
        drop(self.shutdown_tx.take());

        // Wait on the pool thread to finish and pull any errors from it
        let result = self.pool_handle.take()?.join();
        if let Err(payload) = &result {
            self.panic_message = Some(panic_message(&**payload).to_string());
        }
        Some(result)
    }
}

impl Drop for Pool {
    /// Stops the pool if [`Pool::stop_pool`] wasn't called.
    ///
    /// Errors are ignored, so call [`Pool::stop_pool`] to learn whether all output was written.
    /// A panic in one of the pool's threads is propagated, unless the current thread is already
    /// panicking, when a second panic would abort the process.
    fn drop(&mut self) {
        if let Some(Err(payload)) = self.shutdown() {
            if !std::thread::panicking() {
                panic::resume_unwind(payload);
            }
        }
    }
}

////////////////////////////////////////////////////////////////////////////////
// The PoolFailure struct and impls
////////////////////////////////////////////////////////////////////////////////

/// The first failure of any of a [`Pool`]'s threads, shared with its [`PooledWriter`]s so that
/// they fail fast once the pool can no longer produce complete output.
#[derive(Debug, Default)]
struct PoolFailure {
    /// A copy of the first failure, which writers and repeated calls to [`Pool::stop_pool`]
    /// report.  Writers check it before sending each block, so reading it is lock-free.
    first_failure: OnceLock<Failure>,
    /// The first error itself, which the first call to [`Pool::stop_pool`] returns with any
    /// underlying [`io::Error`] intact.
    first_error: Mutex<Option<PoolError>>,
}

impl PoolFailure {
    /// Records `error` if no failure has been recorded yet.
    fn record(&self, error: PoolError) {
        if self.first_failure.set(Failure::Error(error.duplicate())).is_ok() {
            *self.first_error.lock() = Some(error);
        }
    }

    /// Records a thread's panic, given its payload, if no failure has been recorded yet.
    fn record_panic(&self, payload: &(dyn Any + Send)) {
        let _ = self.first_failure.set(Failure::Panic(panic_message(payload).to_string()));
    }

    /// Returns whether a failure has been recorded.
    fn has_failed(&self) -> bool {
        self.first_failure.get().is_some()
    }

    /// Takes the first error, if one was recorded and hasn't been taken already.
    fn take_error(&self) -> Option<PoolError> {
        self.first_error.lock().take()
    }

    /// Returns a copy of the first error, if the first failure was an error.
    fn to_pool_error(&self) -> Option<PoolError> {
        match self.first_failure.get()? {
            Failure::Error(error) => Some(error.duplicate()),
            Failure::Panic(_) => None,
        }
    }

    /// Returns a new [`io::Error`] reporting the first failure, if any.  For an error, it has
    /// the kind of any underlying I/O error and wraps a copy of the [`PoolError`].
    fn to_io_error(&self) -> Option<io::Error> {
        Some(match self.first_failure.get()? {
            Failure::Error(error) => io::Error::new(error.io_error_kind(), error.duplicate()),
            Failure::Panic(message) => {
                io::Error::other(format!("A thread in the pool panicked: {message}"))
            }
        })
    }
}

/// A failure that stops a [`Pool`].
#[derive(Debug)]
enum Failure {
    /// A thread failed with an error.
    Error(PoolError),
    /// A thread panicked with this message.
    Panic(String),
}

/// Returns the message a thread panicked with, given the panic's payload.
fn panic_message(payload: &(dyn Any + Send)) -> &str {
    if let Some(message) = payload.downcast_ref::<&str>() {
        message
    } else if let Some(message) = payload.downcast_ref::<String>() {
        message
    } else {
        "Box<dyn Any>"
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
        io::{BufReader, BufWriter, Read},
        path::{Path, PathBuf},
        vec,
    };

    use crate::bgzf::BgzfCompressor;

    use super::*;
    use ::bgzf::Reader;
    use proptest::prelude::*;
    use tempfile::tempdir;

    /// 128 KB, the buffer size used by pigz, which scales the sizes generated by `test_complete`.
    const BUFSIZE: usize = 128 * 1024;

    /// Marks a block that [`ScriptedCompressor`] fails to compress.
    const FAIL_MARKER: &[u8] = b"FAIL";

    /// Marks a block that [`ScriptedCompressor`] panics on.
    const PANIC_MARKER: &[u8] = b"PANIC";

    fn create_output_writer<P: AsRef<Path>>(path: P) -> BufWriter<File> {
        BufWriter::new(File::create(path).unwrap())
    }

    fn create_output_file_name(name: impl AsRef<Path>, dir: impl AsRef<Path>) -> PathBuf {
        let path = dir.as_ref().to_path_buf();
        path.join(name)
    }

    /// In-memory output that stays readable after its writer has been handed to a pool.
    type SharedOutput = Arc<Mutex<Vec<u8>>>;

    /// Returns the error a test writer fails with.
    fn storage_full() -> io::Error {
        io::Error::new(io::ErrorKind::StorageFull, "No space left on device")
    }

    /// A writer that appends to a [`SharedOutput`], except that the calls to `write` whose
    /// 1-based number is in `failing_calls` fail with "No space left on device".
    struct FlakyWriter {
        output: SharedOutput,
        calls: usize,
        failing_calls: std::ops::Range<usize>,
    }

    impl FlakyWriter {
        /// Creates a writer that fails on `failing_calls`, and returns it with its output.
        fn new(failing_calls: std::ops::Range<usize>) -> (Self, SharedOutput) {
            let output = SharedOutput::default();
            (Self { output: output.clone(), calls: 0, failing_calls }, output)
        }

        /// Creates a writer that never fails, discarding its output.
        fn healthy() -> Self {
            Self::new(0..0).0
        }
    }

    impl Write for FlakyWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.calls += 1;
            if self.failing_calls.contains(&self.calls) {
                return Err(storage_full());
            }
            self.output.lock().extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer that accepts every write but fails to flush with "No space left on device".
    struct UnflushableWriter;

    impl Write for UnflushableWriter {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            Ok(buf.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(storage_full())
        }
    }

    /// A writer whose writes fail with "No space left on device" after a delay, so that a
    /// [`PooledWriter`] sending to it fills the pool's queues first.
    struct SlowFailingWriter;

    impl Write for SlowFailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            std::thread::sleep(Duration::from_millis(100));
            Err(storage_full())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A writer that panics on every write.
    struct PanickingWriter;

    impl Write for PanickingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            panic!("writer panicked");
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    /// A compressor that copies blocks unchanged, except that it fails on a block starting with
    /// [`FAIL_MARKER`] and panics on one starting with [`PANIC_MARKER`], in each case after a
    /// delay so that other threads compress later blocks first.
    struct ScriptedCompressor;

    impl Compressor for ScriptedCompressor {
        type Error = io::Error;
        type CompressionLevel = u8;

        fn new(_compression_level: u8) -> Self {
            Self
        }

        fn default_compression_level() -> u8 {
            0
        }

        fn new_compression_level(compression_level: u8) -> Result<u8, io::Error> {
            Ok(compression_level)
        }

        fn compress(&mut self, input: &[u8], output: &mut Vec<u8>, _: bool) -> io::Result<()> {
            let delay = Duration::from_millis(50);
            if input.starts_with(FAIL_MARKER) {
                std::thread::sleep(delay);
                return Err(io::Error::other("compression failed"));
            }
            if input.starts_with(PANIC_MARKER) {
                std::thread::sleep(delay);
                panic!("compressor panicked");
            }
            output.extend_from_slice(input);
            Ok(())
        }
    }

    /// Returns a builder for a BGZF pool with `threads` threads.
    fn bgzf_builder<W: Write + Send + 'static>(threads: usize) -> PoolBuilder<W, BgzfCompressor> {
        PoolBuilder::new().threads(threads)
    }

    /// Returns a builder for a [`ScriptedCompressor`] pool with `threads` threads.
    fn scripted_builder<W: Write + Send + 'static>(
        threads: usize,
    ) -> PoolBuilder<W, ScriptedCompressor> {
        PoolBuilder::new().threads(threads)
    }

    /// Exchanges each of `writers` with `builder`, then builds the pool.
    fn build_pool<W, C>(
        mut builder: PoolBuilder<W, C>,
        writers: Vec<W>,
    ) -> (Vec<PooledWriter>, Pool)
    where
        W: Write + Send + 'static,
        C: Compressor,
    {
        let pooled_writers = writers.into_iter().map(|w| builder.exchange(w)).collect();
        (pooled_writers, builder.build().unwrap())
    }

    /// Writes full blocks to `writer` until a write fails, and returns the error.
    fn write_until_error(writer: &mut PooledWriter) -> io::Error {
        let block = vec![b'A'; BgzfCompressor::BLOCK_SIZE];
        for _ in 0..10_000 {
            if let Err(error) = writer.write_all(&block) {
                return error;
            }
        }
        panic!("Writes did not fail.");
    }

    /// Returns whether `f` panics.
    fn panics(f: impl FnOnce()) -> bool {
        panic::catch_unwind(AssertUnwindSafe(f)).is_err()
    }

    /// Runs `f` on a new thread and returns its result, failing the test if it takes more than
    /// 30 seconds, so that a hung pool fails its test instead of stalling the whole run.
    fn within_timeout<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = flume::bounded(1);
        std::thread::spawn(move || tx.send(f()));
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(result) => result,
            Err(flume::RecvTimeoutError::Timeout) => panic!("Timed out: the pool probably hung"),
            Err(flume::RecvTimeoutError::Disconnected) => panic!("The timed thread panicked"),
        }
    }

    /// Returns the 28-byte block that marks the end of a BGZF stream.
    fn bgzf_eof_block() -> Vec<u8> {
        let mut eof = Vec::new();
        ::bgzf::Compressor::append_eof(&mut eof);
        eof
    }

    /// Counts the BGZF EOF blocks anywhere in `bytes`.
    fn count_eof_blocks(bytes: &[u8]) -> usize {
        let eof = bgzf_eof_block();
        bytes.windows(eof.len()).filter(|window| *window == eof.as_slice()).count()
    }

    /// Decompresses a BGZF stream.
    fn decompress(bytes: &[u8]) -> Vec<u8> {
        let mut decompressed = Vec::new();
        Reader::new(bytes).read_to_end(&mut decompressed).unwrap();
        decompressed
    }

    /// Returns a few blocks' worth of varied bytes, ending in a partial block.
    fn test_data() -> Vec<u8> {
        (0..3 * BgzfCompressor::BLOCK_SIZE + 100).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn writer_index_is_the_order_in_which_writers_were_exchanged() {
        let writers = vec![FlakyWriter::healthy(), FlakyWriter::healthy()];
        let (pooled_writers, _pool) = build_pool(bgzf_builder(1), writers);

        let indices: Vec<usize> = pooled_writers.iter().map(PooledWriter::writer_index).collect();

        assert_eq!(indices, vec![0, 1]);
    }

    #[test]
    fn closed_writer_output_is_complete_and_has_exactly_one_eof_block() {
        let (writer, output) = FlakyWriter::new(0..0);
        let (mut pooled_writers, mut pool) = build_pool(bgzf_builder(4), vec![writer]);
        let mut pooled_writer = pooled_writers.pop().unwrap();
        let data = test_data();

        pooled_writer.write_all(&data).unwrap();
        pooled_writer.close().unwrap();
        pool.stop_pool().unwrap();

        let output = output.lock();
        assert_eq!(count_eof_blocks(&output), 1);
        assert!(output.ends_with(&bgzf_eof_block()));
        assert_eq!(decompress(&output), data);
    }

    #[test]
    fn dropped_writer_output_is_complete_and_has_exactly_one_eof_block() {
        let (writer, output) = FlakyWriter::new(0..0);
        let (mut pooled_writers, mut pool) = build_pool(bgzf_builder(4), vec![writer]);
        let mut pooled_writer = pooled_writers.pop().unwrap();
        let data = test_data();

        pooled_writer.write_all(&data).unwrap();
        drop(pooled_writer);
        pool.stop_pool().unwrap();

        let output = output.lock();
        assert_eq!(count_eof_blocks(&output), 1);
        assert!(output.ends_with(&bgzf_eof_block()));
        assert_eq!(decompress(&output), data);
    }

    #[test]
    fn stop_pool_returns_the_write_error_when_one_write_fails_among_several_threads() {
        let (writer, _output) = FlakyWriter::new(3..4);
        let (mut pooled_writers, mut pool) = build_pool(bgzf_builder(4), vec![writer]);
        let mut pooled_writer = pooled_writers.pop().unwrap();

        // Whether these see the failure depends on timing, so only stop_pool's result is checked.
        let _ = pooled_writer.write_all(&vec![b'A'; 8 * BgzfCompressor::BLOCK_SIZE]);
        let _ = pooled_writer.close();

        match pool.stop_pool() {
            Err(PoolError::WriterIo { writer_index, source }) => {
                assert_eq!(writer_index, 0);
                assert_eq!(source.kind(), io::ErrorKind::StorageFull);
                assert_eq!(source.to_string(), "No space left on device");
            }
            other => panic!("Expected a write error, got {other:?}"),
        }
    }

    #[test]
    fn stop_pool_returns_the_same_error_when_called_again_after_a_failure() {
        let (writer, _output) = FlakyWriter::new(1..usize::MAX);
        let (mut pooled_writers, mut pool) = build_pool(bgzf_builder(2), vec![writer]);
        write_until_error(&mut pooled_writers[0]);

        let first = pool.stop_pool().unwrap_err();
        let second = pool.stop_pool().unwrap_err();

        assert!(matches!(second, PoolError::WriterIo { writer_index: 0, .. }));
        assert_eq!(second.to_string(), first.to_string());
    }

    #[test]
    fn stop_pool_returns_ok_when_called_again() {
        let (pooled_writers, mut pool) = build_pool(bgzf_builder(2), vec![FlakyWriter::healthy()]);
        pooled_writers.into_iter().try_for_each(PooledWriter::close).unwrap();

        pool.stop_pool().unwrap();
        pool.stop_pool().unwrap();
    }

    #[test]
    fn writes_return_the_write_error_once_the_pool_has_failed() {
        let (writer, _output) = FlakyWriter::new(1..usize::MAX);
        let (mut pooled_writers, _pool) = build_pool(bgzf_builder(2), vec![writer]);

        let error = write_until_error(&mut pooled_writers[0]);

        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert_eq!(error.to_string(), "Failed to write to writer 0: No space left on device");
    }

    #[test]
    fn close_after_the_pool_has_failed_returns_the_write_error() {
        let (writer, _output) = FlakyWriter::new(1..usize::MAX);
        let (mut pooled_writers, mut pool) = build_pool(bgzf_builder(1), vec![writer]);
        let mut pooled_writer = pooled_writers.pop().unwrap();

        pooled_writer.write_all(&vec![b'A'; BgzfCompressor::BLOCK_SIZE]).unwrap();
        assert!(pool.stop_pool().is_err());
        let error = pooled_writer.close().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert_eq!(error.to_string(), "Failed to write to writer 0: No space left on device");
    }

    #[test]
    fn close_on_a_healthy_writer_returns_an_error_naming_the_writer_that_failed() {
        let (failing_writer, _output) = FlakyWriter::new(1..usize::MAX);
        let writers = vec![failing_writer, FlakyWriter::healthy()];
        let (mut pooled_writers, _pool) = build_pool(bgzf_builder(2), writers);
        write_until_error(&mut pooled_writers[0]);

        let error = pooled_writers.pop().unwrap().close().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert_eq!(error.to_string(), "Failed to write to writer 0: No space left on device");
        let pool_error = error.get_ref().and_then(|e| e.downcast_ref::<PoolError>());
        assert!(matches!(pool_error, Some(PoolError::WriterIo { writer_index: 0, .. })));
    }

    #[test]
    fn closing_writers_in_a_loop_after_a_failure_returns_the_write_error() {
        let (failing_writer, _output) = FlakyWriter::new(1..usize::MAX);
        let writers = vec![failing_writer, FlakyWriter::healthy(), FlakyWriter::healthy()];
        let (mut pooled_writers, mut pool) = build_pool(bgzf_builder(2), writers);
        write_until_error(&mut pooled_writers[0]);

        // The first close fails, so try_for_each drops the remaining writers without closing them.
        let error = pooled_writers.into_iter().try_for_each(PooledWriter::close).unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
        assert!(pool.stop_pool().is_err());
    }

    #[test]
    fn flush_returns_the_error_once_the_pool_has_failed() {
        let (failing_writer, _output) = FlakyWriter::new(1..usize::MAX);
        let writers = vec![failing_writer, FlakyWriter::healthy()];
        let (mut pooled_writers, _pool) = build_pool(bgzf_builder(2), writers);
        pooled_writers[1].write_all(b"less than a block").unwrap();
        write_until_error(&mut pooled_writers[0]);

        let error = pooled_writers[1].flush().unwrap_err();

        assert_eq!(error.to_string(), "Failed to write to writer 0: No space left on device");
    }

    #[test]
    fn other_writers_output_is_written_and_flushed_when_one_writer_fails() {
        let (failing_writer, _output) = FlakyWriter::new(1..usize::MAX);
        let (healthy_writer, healthy_output) = FlakyWriter::new(0..0);
        // The BufWriter holds back the healthy writer's output until it is flushed.
        let writers: Vec<Box<dyn Write + Send>> =
            vec![Box::new(failing_writer), Box::new(BufWriter::new(healthy_writer))];
        // A single thread writes blocks in the order they were sent, so every block of the
        // healthy writer is written before the failing writer's first block.
        let (mut pooled_writers, mut pool) = build_pool(bgzf_builder(1), writers);
        let mut healthy_pooled_writer = pooled_writers.pop().unwrap();
        let mut failing_pooled_writer = pooled_writers.pop().unwrap();
        let data = test_data();

        healthy_pooled_writer.write_all(&data).unwrap();
        healthy_pooled_writer.close().unwrap();
        let _ = failing_pooled_writer.write_all(&vec![b'A'; BgzfCompressor::BLOCK_SIZE]);
        let _ = failing_pooled_writer.close();

        assert!(matches!(pool.stop_pool(), Err(PoolError::WriterIo { writer_index: 0, .. })));
        assert_eq!(decompress(&healthy_output.lock()), data);
    }

    #[test]
    fn writer_blocked_on_a_full_queue_returns_the_error_instead_of_hanging() {
        let error = within_timeout(|| {
            let builder = bgzf_builder(1).queue_size(1);
            let (mut pooled_writers, _pool) = build_pool(builder, vec![SlowFailingWriter]);
            write_until_error(&mut pooled_writers[0])
        });

        assert_eq!(error.kind(), io::ErrorKind::StorageFull);
    }

    #[test]
    fn stop_pool_returns_the_error_when_flushing_a_writer_fails() {
        let (mut pooled_writers, mut pool) = build_pool(bgzf_builder(2), vec![UnflushableWriter]);
        let mut pooled_writer = pooled_writers.pop().unwrap();

        // The pool flushes writers only once it stops, so the writer closes successfully.
        pooled_writer.write_all(&test_data()).unwrap();
        pooled_writer.close().unwrap();

        match pool.stop_pool() {
            Err(PoolError::WriterIo { writer_index, source }) => {
                assert_eq!(writer_index, 0);
                assert_eq!(source.kind(), io::ErrorKind::StorageFull);
            }
            other => panic!("Expected a write error, got {other:?}"),
        }
    }

    #[test]
    fn stop_pool_returns_the_compression_error_naming_the_writer() {
        let writers = vec![FlakyWriter::healthy(), FlakyWriter::healthy()];
        let (mut pooled_writers, mut pool) = build_pool(scripted_builder(2), writers);

        pooled_writers[1].write_all(FAIL_MARKER).unwrap();
        for pooled_writer in pooled_writers {
            let _ = pooled_writer.close();
        }

        match pool.stop_pool() {
            Err(PoolError::WriterCompression { writer_index, message }) => {
                assert_eq!(writer_index, 1);
                assert_eq!(message, "compression failed");
            }
            other => panic!("Expected a compression error, got {other:?}"),
        }
    }

    #[test]
    fn stop_pool_returns_the_compression_error_when_a_thread_is_waiting_to_write_the_block() {
        let (mut pooled_writers, mut pool) =
            build_pool(scripted_builder(4), vec![FlakyWriter::healthy()]);
        let mut pooled_writer = pooled_writers.pop().unwrap();
        let mut failing_block = FAIL_MARKER.to_vec();
        failing_block.resize(ScriptedCompressor::BLOCK_SIZE, b'A');

        // The failing block is compressed slowly, so the blocks after it are compressed first
        // and a thread waits for the failing block in order to write them.
        pooled_writer.write_all(&failing_block).unwrap();
        let _ = pooled_writer.write_all(&vec![b'A'; 4 * ScriptedCompressor::BLOCK_SIZE]);
        let _ = pooled_writer.close();

        assert!(matches!(
            pool.stop_pool(),
            Err(PoolError::WriterCompression { writer_index: 0, .. })
        ));
    }

    #[test]
    fn stop_pool_returns_an_error_naming_a_writer_that_was_not_closed() {
        let writers = vec![FlakyWriter::healthy(), FlakyWriter::healthy(), FlakyWriter::healthy()];
        let (mut pooled_writers, mut pool) = build_pool(bgzf_builder(2), writers);
        let mut open_writer = pooled_writers.remove(1);
        open_writer.write_all(b"never written").unwrap();
        pooled_writers.into_iter().try_for_each(PooledWriter::close).unwrap();

        let result = pool.stop_pool();

        assert!(matches!(result, Err(PoolError::WriterNotClosed { writer_index: 1 })));
        drop(open_writer);
    }

    #[test]
    fn close_after_stop_pool_returns_the_not_closed_error() {
        let (mut pooled_writers, mut pool) =
            build_pool(bgzf_builder(2), vec![FlakyWriter::healthy()]);
        let mut pooled_writer = pooled_writers.pop().unwrap();

        pooled_writer.write_all(b"never written").unwrap();
        assert!(pool.stop_pool().is_err());
        let error = pooled_writer.close().unwrap_err();

        assert_eq!(error.to_string(), "Writer 0 was not closed before the pool was stopped");
    }

    #[test]
    fn dropping_writers_and_pool_after_a_failure_does_not_panic() {
        let (writer, _output) = FlakyWriter::new(1..usize::MAX);
        let (mut pooled_writers, pool) = build_pool(bgzf_builder(2), vec![writer]);
        write_until_error(&mut pooled_writers[0]);

        drop(pooled_writers);
        drop(pool);
    }

    #[test]
    #[should_panic(expected = "writer panicked")]
    fn stop_pool_propagates_a_panic_in_a_writer() {
        let (mut pooled_writers, mut pool) = build_pool(bgzf_builder(2), vec![PanickingWriter]);
        let _ = pooled_writers.pop().unwrap().close();

        let _ = pool.stop_pool();
    }

    #[test]
    #[should_panic(expected = "writer panicked")]
    fn dropping_the_pool_propagates_a_panic_in_a_writer() {
        let (mut pooled_writers, pool) = build_pool(bgzf_builder(2), vec![PanickingWriter]);
        let _ = pooled_writers.pop().unwrap().close();

        drop(pool);
    }

    #[test]
    fn stop_pool_panics_again_when_called_again_after_a_thread_panicked() {
        let (mut pooled_writers, mut pool) = build_pool(bgzf_builder(2), vec![PanickingWriter]);
        let _ = pooled_writers.pop().unwrap().close();

        assert!(panics(|| {
            let _ = pool.stop_pool();
        }));
        assert!(panics(|| {
            let _ = pool.stop_pool();
        }));
    }

    #[test]
    fn writes_return_an_error_once_a_thread_has_panicked() {
        let (mut pooled_writers, pool) = build_pool(bgzf_builder(2), vec![PanickingWriter]);

        let error = write_until_error(&mut pooled_writers[0]);

        assert_eq!(error.to_string(), "A thread in the pool panicked: writer panicked");
        assert!(panics(|| drop(pool)));
    }

    #[test]
    fn writes_report_a_compressor_panic_when_a_thread_is_waiting_to_write_the_block() {
        let (mut pooled_writers, pool) =
            build_pool(scripted_builder(4), vec![FlakyWriter::healthy()]);
        let mut panicking_block = PANIC_MARKER.to_vec();
        panicking_block.resize(ScriptedCompressor::BLOCK_SIZE, b'A');

        // The panicking block is compressed slowly, so the blocks after it are compressed first
        // and a thread waits for the panicking block in order to write them.
        pooled_writers[0].write_all(&panicking_block).unwrap();
        let error = write_until_error(&mut pooled_writers[0]);

        assert_eq!(error.to_string(), "A thread in the pool panicked: compressor panicked");
        assert!(panics(|| drop(pool)));
    }

    #[test]
    fn stop_pool_panics_instead_of_hanging_when_compressing_a_final_block_panics() {
        let panicked = within_timeout(|| {
            let (mut pooled_writers, mut pool) =
                build_pool(scripted_builder(2), vec![FlakyWriter::healthy()]);
            let mut pooled_writer = pooled_writers.pop().unwrap();
            pooled_writer.write_all(PANIC_MARKER).unwrap();
            let _ = pooled_writer.close();
            panics(|| {
                let _ = pool.stop_pool();
            })
        });

        assert!(panicked);
    }

    #[test]
    fn test_simple() {
        let dir = tempdir().unwrap();
        let output_names: Vec<PathBuf> = (0..20)
            .map(|i| create_output_file_name(format!("test.{}.txt.gz", i), dir.path()))
            .collect();

        let output_writers: Vec<BufWriter<File>> =
            output_names.iter().map(create_output_writer).collect();
        let mut builder =
            PoolBuilder::<_, BgzfCompressor>::new().threads(8).compression_level(2).unwrap();
        let mut pooled_writers: Vec<PooledWriter> =
            output_writers.into_iter().map(|w| builder.exchange(w)).collect();
        let mut pool = builder.build().unwrap();

        for (i, writer) in pooled_writers.iter_mut().enumerate() {
            writer.write_all(format!("This is writer {}.", i).as_bytes()).unwrap();
        }
        pooled_writers.into_iter().try_for_each(|mut w| w.flush()).unwrap();
        pool.stop_pool().unwrap();

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
            num_output_files in 1..2*num_cpus::get(),
            threads in 1..=2+num_cpus::get(),
            comp_level in 1..=8_u8,
            write_size in 1..=2*BUFSIZE,
        ) {
            let dir = tempdir().unwrap();
            let output_names: Vec<PathBuf> = (0..num_output_files)
                .map(|i| create_output_file_name(format!("test.{}.txt.gz", i), dir.path()))
                .collect();
            let output_writers: Vec<_> = output_names.iter().map(create_output_writer).collect();

            let mut builder = PoolBuilder::<_, BgzfCompressor>::new()
                .threads(threads)
                .compression_level(comp_level)?;

            let mut pooled_writers: Vec<_> = output_writers.into_iter().map(|w| builder.exchange(w)).collect();
            let mut pool = builder.build()?;

            let inputs: Vec<Vec<u8>> = (0..num_output_files).map(|_| {
                (0..input_size).map(|_| rand::random::<u8>()).collect()
            }).collect();

            let chunks = (input_size as f64 / write_size as f64).ceil() as usize;

            // write a chunk to each writer (could randomly select the writers?)
            for i in 0..chunks {
                for (j, writer) in pooled_writers.iter_mut().enumerate() {
                    let input = &inputs[j];
                    let bytes = &input[write_size * i..std::cmp::min(write_size * (i + 1), input.len())];
                    writer.write_all(bytes).unwrap()
                }
            }

            pooled_writers.into_iter().try_for_each(|mut w| w.flush()).unwrap();
            pool.stop_pool().unwrap();

            for (i, path) in output_names.iter().enumerate() {
                let mut reader = Reader::new(BufReader::new(File::open(path).unwrap()));
                let mut actual = vec![];
                reader.read_to_end(&mut actual).unwrap();
                assert_eq!(actual, inputs[i]);
            }

        }
    }
}
