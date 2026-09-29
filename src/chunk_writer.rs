use std::{
    io::{self, Cursor, IoSlice, Write as _},
    pin::Pin,
    task::{Context, Poll, ready},
};

use futures::{AsyncRead, AsyncWrite};

/// Default maximum payload size of a single HTTP chunk.
///
/// This only controls HTTP chunk framing. It has no relation to multipart
/// boundaries or to the total body size.
pub const DEFAULT_MAX_CHUNK_SIZE: usize = 64 * 1024;

/// Final marker of an HTTP/1.1 chunked transfer.
const FINAL_CHUNK: &[u8] = b"0\r\n\r\n";

/// Bound the stack space used for vectored payloads.
const MAX_PAYLOAD_BUFS: usize = 16;

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

    /// Unwritten remainder of an HTTP chunk.
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
        assert!(max_chunk_size > 0, "chunk size must be greater than zero");

        Self {
            inner,

            // Allocate only after a partial write, then reuse the capacity.
            pending: Vec::new(),
            pending_pos: 0,

            max_chunk_size,

            state: State::Writing,
            final_pos: 0,
        }
    }
}

impl<T: AsyncWrite + Unpin> ChunkWriter<T> {
    /// Try to completely write the currently buffered encoded chunk.
    fn poll_drain_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        while self.pending_pos < self.pending.len() {
            match Pin::new(&mut self.inner).poll_write(cx, &self.pending[self.pending_pos..]) {
                Poll::Pending => {
                    return Poll::Pending;
                }

                Poll::Ready(Err(e)) => {
                    return Poll::Ready(Err(e));
                }

                Poll::Ready(Ok(0)) => {
                    return Poll::Ready(Err(io::Error::from(io::ErrorKind::WriteZero)));
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
        self.poll_write_vectored(cx, &[IoSlice::new(buf)])
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

        // Finish the previous chunk before accepting another payload.
        ready!(this.poll_drain_pending(cx))?;

        let mut chunk = [IoSlice::new(&[]); MAX_PAYLOAD_BUFS + 2];
        let mut count = 1;
        let mut payload_len = 0;

        for buf in bufs {
            if payload_len == this.max_chunk_size || count == MAX_PAYLOAD_BUFS + 1 {
                break;
            }
            let n = buf.len().min(this.max_chunk_size - payload_len);
            if n > 0 {
                chunk[count] = IoSlice::new(&buf[..n]);
                count += 1;
                payload_len += n;
            }
        }

        if payload_len == 0 {
            return Poll::Ready(Ok(0));
        }

        let mut header = [0; 2 * size_of::<usize>() + 2];
        let header_len = {
            let mut cursor = Cursor::new(&mut header[..]);
            write!(&mut cursor, "{:X}\r\n", payload_len)?;
            cursor.position() as usize
        };
        let chunk_len = payload_len.checked_add(header_len + 2).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "chunk size overflows usize")
        })?;
        chunk[0] = IoSlice::new(&header[..header_len]);
        chunk[count] = IoSlice::new(b"\r\n");

        let written =
            ready!(Pin::new(&mut this.inner).poll_write_vectored(cx, &chunk[..count + 1]))?;
        if written == 0 {
            return Poll::Ready(Err(io::ErrorKind::WriteZero.into()));
        }

        // Once the header starts, retain the rest of the announced chunk so
        // the caller can reuse its buffers or close the writer immediately.
        if written < chunk_len {
            let mut remaining = &mut chunk[..count + 1];
            IoSlice::advance_slices(&mut remaining, written);
            this.pending.reserve(chunk_len - written);
            for buf in remaining {
                this.pending.extend_from_slice(buf);
            }
        }

        Poll::Ready(Ok(payload_len))
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
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

    fn poll_close(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();

        loop {
            match this.state {
                /*
                 * Finish any normal data chunk before sending the
                 * terminating zero-length chunk.
                 */
                State::Writing => match this.poll_drain_pending(cx) {
                    Poll::Pending => {
                        return Poll::Pending;
                    }

                    Poll::Ready(Err(e)) => {
                        return Poll::Ready(Err(e));
                    }

                    Poll::Ready(Ok(())) => {
                        this.state = State::Finalizing;
                    }
                },

                /*
                 * Send:
                 *
                 *     0\r\n\r\n
                 *
                 * directly to the underlying transport.
                 *
                 * It must NOT go through poll_write(), otherwise we'd
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
                                return Poll::Ready(Err(io::Error::from(io::ErrorKind::WriteZero)));
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
                State::Flushing => match Pin::new(&mut this.inner).poll_flush(cx) {
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
                },

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
