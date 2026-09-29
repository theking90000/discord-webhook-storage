use std::{
    io::{self, IoSlice, Write as _},
    pin::Pin,
    task::{Context, Poll},
};

use futures::{AsyncRead, AsyncWrite};

/// Default maximum payload size of a single HTTP chunk.
///
/// This only controls HTTP chunk framing. It has no relation to multipart
/// boundaries or to the total body size.
pub const DEFAULT_MAX_CHUNK_SIZE: usize = 64 * 1024;

/// Final marker of an HTTP/1.1 chunked transfer.
const FINAL_CHUNK: &[u8] = b"0\r\n\r\n";

/// An [`AsyncWrite`] adapter implementing HTTP/1.1
/// `Transfer-Encoding: chunked`.
///
/// Bytes written to this writer:
///
/// ```text
/// hello
/// ```
///
/// are encoded on the underlying stream as:
///
/// ```text
/// 5\r\n
/// hello\r\n
/// ```
///
/// Calling [`AsyncWrite::poll_close`] finishes the chunked transfer by
/// writing:
///
/// ```text
/// 0\r\n
/// \r\n
/// ```
///
/// It deliberately does NOT close the underlying stream, since an HTTP
/// client normally needs to read the server response afterwards.
#[derive(Debug)]
pub struct ChunkWriter<T> {
    inner: T,

    /// Fully encoded HTTP chunk waiting to be written.
    pending: Vec<u8>,

    /// Number of bytes from `pending` already written to `inner`.
    pending_pos: usize,

    /// Maximum payload size for a single chunk.
    max_chunk_size: usize,

    state: State,

    /// Number of bytes of `FINAL_CHUNK` already written.
    final_pos: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Writing,
    Finalizing,
    Flushing,
    Closed,
}

impl<T> ChunkWriter<T> {
    /// Create a chunked writer using [`DEFAULT_MAX_CHUNK_SIZE`].
    pub fn new(inner: T) -> Self {
        Self::with_max_chunk_size(inner, DEFAULT_MAX_CHUNK_SIZE)
    }

    /// Create a chunked writer with a custom maximum chunk payload size.
    ///
    /// # Panics
    ///
    /// Panics if `max_chunk_size == 0`.
    pub fn with_max_chunk_size(inner: T, max_chunk_size: usize) -> Self {
        assert!(
            max_chunk_size > 0,
            "chunk size must be greater than zero"
        );

        Self {
            inner,

            // A small initial allocation. The Vec will grow when needed
            // and then reuse its capacity for subsequent chunks.
            pending: Vec::new(),
            pending_pos: 0,

            max_chunk_size,

            state: State::Writing,
            final_pos: 0,
        }
    }

    /// Return a reference to the underlying transport.
    pub fn get_ref(&self) -> &T {
        &self.inner
    }

    /// Return a mutable reference to the underlying transport.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.inner
    }

    /// Consume the chunk writer and return the underlying transport.
    ///
    /// This does not automatically finish the chunked transfer.
    /// The caller should normally call `close().await` first.
    pub fn into_inner(self) -> T {
        self.inner
    }

    /// Returns whether the terminating zero-length chunk has been sent
    /// and the underlying stream flushed.
    pub fn is_closed(&self) -> bool {
        self.state == State::Closed
    }

    /// Prepare one encoded HTTP chunk in `pending`.
    fn prepare_chunk(&mut self, data: &[u8]) -> io::Result<()> {
        debug_assert!(!data.is_empty());
        debug_assert!(data.len() <= self.max_chunk_size);
        debug_assert!(self.pending.is_empty());

        self.pending_pos = 0;

        // Vec<u8> implements std::io::Write, so this avoids allocating
        // an intermediate String with format!("{:X}\r\n", ...).
        write!(&mut self.pending, "{:X}\r\n", data.len())?;

        self.pending.extend_from_slice(data);
        self.pending.extend_from_slice(b"\r\n");

        Ok(())
    }

    /// Same as `prepare_chunk`, but consumes bytes from several IoSlices.
    ///
    /// Returns the number of payload bytes accepted.
    fn prepare_vectored_chunk(
        &mut self,
        bufs: &[IoSlice<'_>],
    ) -> io::Result<usize> {
        debug_assert!(self.pending.is_empty());

        // Determine how many input bytes we want to accept without ever
        // overflowing usize.
        let mut payload_len = 0;

        for buf in bufs {
            let available = self.max_chunk_size - payload_len;

            if available == 0 {
                break;
            }

            payload_len += buf.len().min(available);
        }

        if payload_len == 0 {
            return Ok(0);
        }

        self.pending_pos = 0;

        write!(&mut self.pending, "{:X}\r\n", payload_len)?;

        let mut remaining = payload_len;

        for buf in bufs {
            if remaining == 0 {
                break;
            }

            let n = buf.len().min(remaining);

            self.pending.extend_from_slice(&buf[..n]);

            remaining -= n;
        }

        self.pending.extend_from_slice(b"\r\n");

        Ok(payload_len)
    }
}

impl<T: AsyncWrite + Unpin> ChunkWriter<T> {
    /// Try to completely write the currently buffered encoded chunk.
    fn poll_drain_pending(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        while self.pending_pos < self.pending.len() {
            match Pin::new(&mut self.inner)
                .poll_write(cx, &self.pending[self.pending_pos..])
            {
                Poll::Pending => {
                    return Poll::Pending;
                }

                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Err(e));
                }

                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::from(
                        io::ErrorKind::WriteZero,
                    )));
                }

                Poll::Ready(Ok(n)) => {
                    self.pending_pos += n;
                }
            }
        }

        self.pending.clear();
        self.pending_pos = 0;

        Poll::Ready(Ok(()))
    }
}

impl<T: AsyncWrite + Unpin> AsyncWrite for ChunkWriter<T> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if this.state != State::Writing {
            return Poll::Ready(Err(io::Error::other(
                "chunked stream is no longer writable",
            )));
        }

        /*
         * Before accepting more bytes from the caller, the previous
         * encoded chunk must have been completely written.
         */
        match this.poll_drain_pending(cx) {
            Poll::Pending => {
                return Poll::Pending;
            }

            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(e));
            }

            Poll::Ready(Ok(())) => {}
        }

        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let n = buf.len().min(this.max_chunk_size);

        /*
         * Copy the caller's bytes into our own buffer.
         *
         * Once copied, returning Ok(n) is valid even if the corresponding
         * encoded chunk has not yet reached the underlying transport:
         * ChunkWriter now owns those n bytes.
         */
        if let Err(e) = this.prepare_chunk(&buf[..n]) {
            return Poll::Ready(Err(e));
        }

        Poll::Ready(Ok(n))
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        if this.state != State::Writing {
            return Poll::Ready(Err(io::Error::other(
                "chunked stream is no longer writable",
            )));
        }

        match this.poll_drain_pending(cx) {
            Poll::Pending => {
                return Poll::Pending;
            }

            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(e));
            }

            Poll::Ready(Ok(())) => {}
        }

        match this.prepare_vectored_chunk(bufs) {
            Ok(n) => Poll::Ready(Ok(n)),
            Err(e) => Poll::Ready(Err(e)),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        // First make sure the last accepted payload was actually sent.
        match this.poll_drain_pending(cx) {
            Poll::Pending => {
                return Poll::Pending;
            }

            Poll::Ready(Err(e)) => {
                return Poll::Ready(Err(e));
            }

            Poll::Ready(Ok(())) => {}
        }

        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_close(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        loop {
            match this.state {
                /*
                 * Finish any normal data chunk before sending the
                 * terminating zero-length chunk.
                 */
                State::Writing => {
                    match this.poll_drain_pending(cx) {
                        Poll::Pending => {
                            return Poll::Pending;
                        }

                        Poll::Ready(Err(e)) => {
                            return Poll::Ready(Err(e));
                        }

                        Poll::Ready(Ok(())) => {
                            this.state = State::Finalizing;
                        }
                    }
                }

                /*
                 * Send:
                 *
                 *     0\r\n\r\n
                 *
                 * directly to the underlying transport.
                 *
                 * It must NOT go through prepare_chunk(), otherwise we'd
                 * chunk-encode the chunk terminator itself.
                 */
                State::Finalizing => {
                    while this.final_pos < FINAL_CHUNK.len() {
                        match Pin::new(&mut this.inner)
                            .poll_write(cx, &FINAL_CHUNK[this.final_pos..])
                        {
                            Poll::Pending => {
                                return Poll::Pending;
                            }

                            Poll::Ready(Err(e)) => {
                                return Poll::Ready(Err(e));
                            }

                            Poll::Ready(Ok(0)) => {
                                return Poll::Ready(Err(io::Error::from(
                                    io::ErrorKind::WriteZero,
                                )));
                            }

                            Poll::Ready(Ok(n)) => {
                                this.final_pos += n;
                            }
                        }
                    }

                    this.state = State::Flushing;
                }

                /*
                 * Flush but deliberately DO NOT call inner.poll_close().
                 *
                 * The HTTP request is finished, but the TCP/TLS stream
                 * must remain alive so that its response can be read.
                 */
                State::Flushing => {
                    match Pin::new(&mut this.inner).poll_flush(cx) {
                        Poll::Pending => {
                            return Poll::Pending;
                        }

                        Poll::Ready(Err(e)) => {
                            return Poll::Ready(Err(e));
                        }

                        Poll::Ready(Ok(())) => {
                            this.state = State::Closed;

                            return Poll::Ready(Ok(()));
                        }
                    }
                }

                State::Closed => {
                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

/// Pass reads straight through to the underlying transport.
///
/// This is useful for HTTP clients because, after the request body has been
/// finalized with `close()`, the same TCP/TLS connection can immediately be
/// used to read the HTTP response.
impl<T: AsyncRead + Unpin> AsyncRead for ChunkWriter<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();

        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}