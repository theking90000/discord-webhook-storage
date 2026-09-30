use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
};

use crate::{
    DiscordFileUrl, DiscordFileUrlError, Error, HttpError, Result, http::HttpStatusParser,
};
use futures::{AsyncRead, AsyncWrite, AsyncWriteExt};

const MAX_ERROR_BODY_SIZE: usize = 16384;

/// An open attachment download implementing [`AsyncRead`].
///
/// Reads stop at the response's `Content-Length`, returning EOF without waiting
/// for the server to close the connection. A transport EOF before that length
/// returns [`io::ErrorKind::UnexpectedEof`]. Transport errors pass through unchanged.
///
/// The transport must already be connected to `cdn.discordapp.com:443` over TCP
/// with the TLS handshake complete, using HTTP/1.1. Pass `&mut connection` to
/// [`Self::open`] to retain ownership of the connection. After reading the entire
/// body, drop the reader to release the borrow. The connection can then be reused
/// if the server keeps it open. An owned connection is dropped with the reader.
///
/// Dropping the reader does not drain unread bytes. If it is dropped before the
/// complete body is read, discard the connection. Unread response bytes would
/// otherwise be mistaken for the next response. Also discard it after any I/O
/// or opening error, or if opening is cancelled after sending the request.
pub struct ReadFile<T> {
    connection: T,
    buffered: Vec<u8>,
    buffered_at: usize,
    remaining: usize,
}

impl<T: AsyncWrite + AsyncRead + Unpin> ReadFile<T> {
    /// Send a GET request with `Range: bytes=0-` and read the response headers.
    ///
    /// `url` accepts [`DiscordFileUrl`] or [`crate::DiscordFile`], including their
    /// references, through [`AsRef`]. This borrows the URL during opening without
    /// retaining it in the reader. `connection` uses the `futures` I/O traits and
    /// must already be connected with TLS to `cdn.discordapp.com:443`.
    /// See [`Self::open_with_range`] for response handling and errors.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::open_with_range`].
    pub async fn open(connection: T, url: impl AsRef<DiscordFileUrl>) -> Result<Self> {
        Self::open_with_range(connection, url, 0, None).await
    }

    /// Send a GET request for `start` through the inclusive `end` byte offset.
    ///
    /// `None` requests every byte from `start` to the end of the file. A supplied
    /// `end` must be greater than `start`. Sends `Range: bytes=start-end`, or
    /// `Range: bytes=start-` for `None`, then flushes the request and parses the
    /// response headers before returning the reader.
    ///
    /// Accepts HTTP 206, or HTTP 200 for `start = 0` and `end = None` if the server
    /// ignores the full-file range. Successful responses require `Content-Length`.
    /// The reader retains any body bytes already read with the headers.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDiscordFileUrl`] if [`DiscordFileUrl::is_valid`]
    /// is false, [`Error::InvalidRange`] if `start >= end`, [`Error::IoError`]
    /// for transport failures, or [`Error::HttpParseError`] for response parsing
    /// or framing failures. Unexpected statuses return [`Error::HttpStatus`]
    /// with the response body if it can be decoded within 16 KiB; otherwise
    /// returns the body parsing or transport error. Discard the connection after
    /// an opening error that occurs after the request has been sent.
    pub async fn open_with_range(
        mut connection: T,
        url: impl AsRef<DiscordFileUrl>,
        start: usize,
        end: Option<usize>,
    ) -> Result<Self> {
        let url = url.as_ref();
        if !url.is_valid() {
            return Err(DiscordFileUrlError::Expired.into());
        }
        if let Some(end) = end {
            if start >= end {
                return Err(Error::InvalidRange { start, end });
            }
        }

        // The fields are public, so validate their spelling before placing them
        // in a request line. This also rejects CRLF injection in constructed URLs.
        let url = DiscordFileUrl::parse(&url.to_string())?;
        let range_end = end.map(|end| end.to_string()).unwrap_or_default();
        let request = format!(
            "GET /attachments/{}/{}/{}?ex={:x}&is={:x}&hm={} HTTP/1.1\r\n\
             Host: cdn.discordapp.com\r\n\
             Range: bytes={start}-{range_end}\r\n\
             Accept-Encoding: identity\r\n\
             \r\n",
            url.channel_id, url.attachment_id, url.attachment_name, url.ex, url.is, url.hm
        );
        connection.write_all(request.as_bytes()).await?;
        connection.flush().await?;

        let mut parser = HttpStatusParser::new(connection);
        let status = parser.status().await?;
        let mut headers = parser.into_headers();
        while headers.next_header().await?.is_some() {}

        if status != 206 && !(status == 200 && start == 0 && end.is_none()) {
            let body = headers.body(MAX_ERROR_BODY_SIZE).await?;
            return Err(Error::HttpStatus { status, body });
        }
        let remaining = headers.body_size().ok_or(HttpError::MissingContentLength)?;
        let (connection, buffered) = headers.remaining();
        if buffered.len() > remaining {
            return Err(HttpError::UnexpectedBodyBytes {
                expected: remaining,
                received: buffered.len(),
            }
            .into());
        }
        Ok(Self {
            connection,
            buffered,
            buffered_at: 0,
            remaining,
        })
    }
}

impl<T: AsyncRead + Unpin> AsyncRead for ReadFile<T> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let limit = buf.len().min(this.remaining);
        if limit == 0 {
            return Poll::Ready(Ok(0));
        }
        let buffered = &this.buffered[this.buffered_at..];
        if !buffered.is_empty() {
            let n = limit.min(buffered.len());
            buf[..n].copy_from_slice(&buffered[..n]);
            this.buffered_at += n;
            this.remaining -= n;
            return Poll::Ready(Ok(n));
        }
        match Pin::new(&mut this.connection).poll_read(cx, &mut buf[..limit]) {
            Poll::Ready(Ok(0)) => Poll::Ready(Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "attachment response ended before Content-Length bytes were read",
            ))),
            Poll::Ready(Ok(n)) => {
                this.remaining -= n;
                Poll::Ready(Ok(n))
            }
            result => result,
        }
    }
}
