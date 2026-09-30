//! Incremental HTTP/1.1 response parsing with bounded decoded body sizes.

use crate::{HttpError, HttpPart, Result};
use futures::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt,
    io::{BufReader, Cursor},
};

/// Retain unread bytes across lines, including any response body read ahead.
struct LineParser<T> {
    connection: T,
    buf: Vec<u8>,
    start: usize,
    search_pos: usize,
}

impl<T: AsyncRead + Unpin> LineParser<T> {
    fn new(connection: T) -> Self {
        Self {
            connection,
            buf: Vec::new(),
            start: 0,
            search_pos: 0,
        }
    }

    fn feed(&mut self, data: &[u8]) {
        if self.start >= 4096 && self.start >= self.buf.len() / 2 {
            let remaining = self.buf.len() - self.start;

            self.buf.copy_within(self.start.., 0);
            self.buf.truncate(remaining);

            self.search_pos -= self.start;
            self.start = 0;
        }

        self.buf.extend_from_slice(data);
    }

    async fn next_line(&mut self, part: HttpPart) -> Result<&[u8]> {
        let mut buf = [0; 1024];
        let pos = loop {
            if let Some(relative) = self.buf[self.search_pos..].iter().position(|&b| b == b'\n') {
                break self.search_pos + relative;
            }
            self.search_pos = self.buf.len();

            let n = self.connection.read(&mut buf).await?;
            if n == 0 {
                return Err(HttpError::UnexpectedEof { part }.into());
            }
            self.feed(&buf[..n]);
        };

        let start = self.start;

        self.start = pos + 1;
        self.search_pos = self.start;

        Ok(&self.buf[start..pos])
    }

    fn remaining(mut self) -> (T, Vec<u8>) {
        if self.start > 0 {
            let remaining = self.buf.len() - self.start;

            self.buf.copy_within(self.start.., 0);
            self.buf.truncate(remaining);
        }

        (self.connection, self.buf)
    }
}

/// Parse the status line before transferring buffered input to the header parser.
pub(crate) struct HttpStatusParser<T> {
    lines: LineParser<T>,
}

/// Track response framing while reading headers up to their terminating empty line.
pub(crate) struct HttpHeaderParser<T> {
    lines: LineParser<T>,
    content_length: Option<usize>,
    chunked: bool,
    done: bool,
}

impl<T: AsyncRead + Unpin> HttpStatusParser<T> {
    pub(crate) fn new(connection: T) -> Self {
        Self {
            lines: LineParser::new(connection),
        }
    }

    pub(crate) async fn status(&mut self) -> Result<u16> {
        let line = self.lines.next_line(HttpPart::StatusLine).await?;

        let line = decode_utf8(line, HttpPart::StatusLine)?;

        let rest = line
            .strip_prefix("HTTP/1.1 ")
            .ok_or(HttpError::UnsupportedHttpVersion)?;

        let (status, _) = rest.split_once(' ').ok_or(HttpError::MalformedStatusLine)?;
        let code = status
            .parse::<u16>()
            .map_err(|source| HttpError::InvalidStatusCode {
                value: status.to_owned(),
                source: Some(source),
            })?;
        if status.len() != 3
            || !status.bytes().all(|b| b.is_ascii_digit())
            || !(100..=599).contains(&code)
        {
            return Err(HttpError::InvalidStatusCode {
                value: status.to_owned(),
                source: None,
            }
            .into());
        }
        Ok(code)
    }

    pub(crate) fn into_headers(self) -> HttpHeaderParser<T> {
        HttpHeaderParser {
            lines: self.lines,
            content_length: None,
            chunked: false,
            done: false,
        }
    }
}

impl<T: AsyncRead + Unpin> HttpHeaderParser<T> {
    pub(crate) async fn next_header(&mut self) -> Result<Option<(&str, &str)>> {
        if self.done {
            return Ok(None);
        }

        let line = self.lines.next_line(HttpPart::HeaderLine).await?;

        let line = decode_utf8(line, HttpPart::HeaderLine)?;
        let line = line.strip_suffix('\r').unwrap_or(line);

        // Empty line => end of headers
        if line.is_empty() {
            if self.chunked && self.content_length.is_some() {
                return Err(HttpError::ConflictingBodyFraming.into());
            }
            self.done = true;
            return Ok(None);
        }

        let (name, value) = line.split_once(':').ok_or(HttpError::MalformedHeader)?;

        let value = value.trim_ascii();

        if name.eq_ignore_ascii_case("content-length") {
            let len = value
                .parse::<usize>()
                .map_err(|source| HttpError::InvalidContentLength {
                    value: value.to_owned(),
                    source,
                })?;

            if let Some(previous) = self.content_length {
                if previous != len {
                    return Err(HttpError::ConflictingContentLength {
                        first: previous,
                        second: len,
                    }
                    .into());
                }
            }

            self.content_length = Some(len);
        }

        if name.eq_ignore_ascii_case("transfer-encoding") {
            // Only plain chunked encoding is supported. Other transfer codings
            // would require additional decoding before parsing the JSON body.
            if self.chunked || !value.eq_ignore_ascii_case("chunked") {
                return Err(HttpError::UnsupportedTransferEncoding {
                    value: value.to_owned(),
                }
                .into());
            }
            self.chunked = true;
        }

        Ok(Some((name, value)))
    }

    pub(crate) fn body_size(&self) -> Option<usize> {
        self.content_length
    }

    pub(crate) fn is_chunked(&self) -> bool {
        self.chunked
    }

    pub(crate) fn is_complete(&self) -> bool {
        self.done
    }

    /// Read the response body using the framing specified by its headers.
    pub(crate) async fn body(self, max_size: usize) -> Result<Vec<u8>> {
        if self.is_chunked() {
            let (mut connection, buffered) = self.remaining();
            read_chunked_body(&mut connection, buffered, max_size).await
        } else {
            read_body(self, max_size).await
        }
    }

    pub(crate) fn remaining(self) -> (T, Vec<u8>) {
        self.lines.remaining()
    }
}

/// Read a response body with Content-Length, keeping bytes read with the headers.
pub(crate) async fn read_body<T: AsyncRead + Unpin>(
    parser: HttpHeaderParser<T>,
    max_size: usize,
) -> Result<Vec<u8>> {
    let body_size = parser.body_size().ok_or(HttpError::MissingBodyFraming)?;
    if body_size > max_size {
        return Err(HttpError::BodyTooLarge {
            limit: max_size,
            size: body_size,
        }
        .into());
    }

    let (mut connection, mut body) = parser.remaining();
    if body.len() > body_size {
        return Err(HttpError::UnexpectedBodyBytes {
            expected: body_size,
            received: body.len(),
        }
        .into());
    }
    let already_read = body.len();
    body.resize(body_size, 0);
    connection.read_exact(&mut body[already_read..]).await?;
    Ok(body)
}

// Bound framing metadata independently of the decoded body size.
const MAX_CHUNK_LINE_SIZE: usize = 8192;
const MAX_TRAILERS_SIZE: usize = 16384;

async fn read_chunk_line<T: AsyncBufRead + Unpin>(
    reader: &mut T,
    line: &mut Vec<u8>,
    part: HttpPart,
) -> Result<()> {
    line.clear();
    reader
        .take((MAX_CHUNK_LINE_SIZE + 1) as u64)
        .read_until(b'\n', line)
        .await?;
    if line.len() > MAX_CHUNK_LINE_SIZE {
        return Err(HttpError::MetadataTooLarge {
            part,
            limit: MAX_CHUNK_LINE_SIZE,
        }
        .into());
    }
    if line.is_empty() || !line.ends_with(b"\n") {
        return Err(HttpError::UnexpectedEof { part }.into());
    }
    if !line.ends_with(b"\r\n") {
        return Err(HttpError::InvalidLineEnding { part }.into());
    }
    Ok(())
}

/// Decode a chunked response, starting with body bytes read with the headers.
pub(crate) async fn read_chunked_body<T: AsyncRead + Unpin>(
    connection: &mut T,
    buffered: Vec<u8>,
    max_size: usize,
) -> Result<Vec<u8>> {
    let mut reader = BufReader::with_capacity(1024, Cursor::new(buffered).chain(connection));
    let mut body = Vec::new();
    let mut line = Vec::new();

    loop {
        read_chunk_line(&mut reader, &mut line, HttpPart::ChunkSizeLine).await?;
        let chunk_line = &line[..line.len() - 2];
        let size_end = chunk_line
            .iter()
            .position(|&byte| byte == b';')
            .unwrap_or(chunk_line.len());
        let size_bytes = chunk_line[..size_end].trim_ascii_end();
        if size_bytes.is_empty()
            || size_bytes.len() > 16
            || !size_bytes.iter().all(u8::is_ascii_hexdigit)
            || chunk_line[..size_end]
                .iter()
                .skip(size_bytes.len())
                .any(|&byte| byte != b' ' && byte != b'\t')
            || chunk_line.contains(&b'\r')
        {
            return Err(HttpError::InvalidChunkSize {
                value: String::from_utf8_lossy(chunk_line).into_owned(),
                source: None,
            }
            .into());
        }
        let size_text = decode_utf8(size_bytes, HttpPart::ChunkSizeLine)?;
        let size =
            usize::from_str_radix(size_text, 16).map_err(|source| HttpError::InvalidChunkSize {
                value: String::from_utf8_lossy(chunk_line).into_owned(),
                source: Some(source),
            })?;

        if size == 0 {
            break;
        }
        if size > max_size - body.len() {
            return Err(HttpError::BodyTooLarge {
                limit: max_size,
                size: body.len().saturating_add(size),
            }
            .into());
        }

        let start = body.len();
        body.resize(start + size, 0);
        reader.read_exact(&mut body[start..]).await?;
        let mut delimiter = [0; 2];
        reader.read_exact(&mut delimiter).await?;
        if delimiter != *b"\r\n" {
            return Err(HttpError::InvalidChunkDelimiter.into());
        }
    }

    // The zero chunk is followed by trailers and an empty line, even when
    // no trailers are present. Do not wait for EOF on a persistent connection.
    let mut trailers_size = 0;
    loop {
        read_chunk_line(&mut reader, &mut line, HttpPart::TrailerLine).await?;
        trailers_size += line.len();
        if trailers_size > MAX_TRAILERS_SIZE {
            return Err(HttpError::MetadataTooLarge {
                part: HttpPart::Trailers,
                limit: MAX_TRAILERS_SIZE,
            }
            .into());
        }
        if line == b"\r\n" {
            return Ok(body);
        }
        let trailer = &line[..line.len() - 2];
        let colon = trailer
            .iter()
            .position(|&byte| byte == b':')
            .ok_or(HttpError::MalformedTrailer)?;
        let name = &trailer[..colon];
        let value = &trailer[colon + 1..];
        if name.is_empty()
            || !name
                .iter()
                .all(|&byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
            || !value
                .iter()
                .all(|&byte| byte == b'\t' || (byte >= b' ' && byte != 0x7f))
        {
            return Err(HttpError::MalformedTrailer.into());
        }
    }
}

fn decode_utf8(bytes: &[u8], part: HttpPart) -> Result<&str> {
    std::str::from_utf8(bytes).map_err(|source| HttpError::InvalidUtf8 { part, source }.into())
}
