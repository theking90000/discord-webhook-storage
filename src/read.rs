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

/// A file opened for asynchronous reading from Discord.
///
/// Use the standard [`AsyncRead`] operations to read its contents. Reading
/// returns zero bytes at the end of the file or the selected range. If the
/// download stops early, reading returns [`io::ErrorKind::UnexpectedEof`].
///
/// The caller supplies a secure connection to `cdn.discordapp.com:443` using
/// HTTP/1.1. Pass `&mut connection` to keep ownership of it. After reading to
/// the end, drop the file before reusing the connection, if it is still open.
/// A connection passed by value is dropped with the file.
///
/// Discard a retained connection after an error or an interrupted download.
/// This includes dropping the file before reading to the end, or cancelling
/// opening after it has started sending the request.
pub struct ReadFile<T> {
    connection: T,
    buffered: Vec<u8>,
    buffered_at: usize,
    remaining: usize,
}

impl<T: AsyncWrite + AsyncRead + Unpin> ReadFile<T> {
    /// Open a stored file for reading from the beginning.
    ///
    /// `url` can be a [`crate::DiscordFile`], a [`DiscordFileUrl`], or a reference
    /// to either. It does not need to remain alive after opening.
    ///
    /// Supply a secure connection to `cdn.discordapp.com:443` as described in
    /// [`ReadFile`]. Use [`Self::open_with_range`] to read only part of the file.
    ///
    /// # Errors
    ///
    /// Returns the same errors as [`Self::open_with_range`].
    pub async fn open(connection: T, url: impl AsRef<DiscordFileUrl>) -> Result<Self> {
        Self::open_with_range(connection, url, 0, None).await
    }

    /// Open a stored file for reading a selected range of bytes.
    ///
    /// `start` is the first byte offset, counting from zero. `end` is the last
    /// byte offset to include, and must be greater than `start`. Pass `None` to
    /// read from `start` to the end of the file.
    ///
    /// `url` accepts the same file references as [`Self::open`]. Supply a secure
    /// connection to `cdn.discordapp.com:443` as described in [`ReadFile`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidDiscordFileUrl`] if the file URL is invalid or
    /// expired, or the system clock cannot be checked. Returns
    /// [`Error::InvalidRange`] if a supplied `end` is not greater than `start`.
    /// Connection failures return [`Error::IoError`].
    ///
    /// Returns [`Error::HttpStatus`] if Discord rejects the download, or
    /// [`Error::HttpParseError`] if its response cannot be read. Error responses
    /// are limited to 16 KiB. Discard the connection if opening fails after
    /// sending the request.
    ///
    /// The download requires HTTP 206, or HTTP 200 when reading the whole file,
    /// and a `Content-Length` header.
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
