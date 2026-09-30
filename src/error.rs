use std::{fmt, io, num::ParseIntError, str::Utf8Error};

/// An error while opening, writing, finishing, reading, or renewing a file.
///
/// Match the variants to distinguish invalid file references, connection
/// failures, and errors returned by Discord. Use [`std::error::Error::source`]
/// to inspect the underlying cause when available.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The webhook URL is invalid or lacks the credentials needed to create a file.
    InvalidWebhookUrl(WebhookUrlError),
    /// The file reference or download URL is invalid or expired.
    InvalidDiscordFileUrl(DiscordFileUrlError),
    /// The connection failed. The original I/O error is available for inspection.
    IoError(io::Error),
    /// File settings could not be prepared, or Discord returned unreadable file details.
    JsonError(serde_json::Error),
    /// Discord's response is unreadable, unsupported, or too large.
    HttpParseError(HttpError),
    /// Discord did not accept the file operation.
    HttpStatus {
        /// HTTP status code returned by Discord.
        status: u16,
        /// Discord's error response, which may explain why the operation failed.
        ///
        /// This may contain JSON, text, or other bytes. It is limited to 16 KiB.
        body: Vec<u8>,
    },
    /// Discord's response lacks valid details for the stored file.
    InvalidResponse(ResponseError),
    /// A write failed without accepting any bytes from that call.
    WriteError(WriteError),
    /// The requested last byte offset is not greater than the first.
    InvalidRange {
        /// First requested byte offset.
        start: usize,
        /// Last requested byte offset, included in the range.
        end: usize,
    },
}

/// Why a webhook URL cannot be used to create files.
///
/// These errors do not contain the webhook's secret token.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WebhookUrlError {
    /// The URL does not start with `https://discord.com/api/webhooks/`.
    InvalidPrefix,
    /// The separator between the identifier and token is missing.
    MissingTokenSeparator,
    /// The webhook identifier is empty.
    EmptyId,
    /// The webhook token is empty.
    EmptyToken,
}

/// Why a file reference or download URL cannot be used.
///
/// These errors do not contain the download URL's signature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiscordFileUrlError {
    /// The download URL has expired, or the local clock cannot be checked.
    Expired,
    /// The URL does not start with `https://cdn.discordapp.com/attachments/`.
    InvalidPrefix,
    /// The file reference or URL lacks a required value.
    MissingField {
        /// Name of the missing field.
        field: &'static str,
    },
    /// A value in the file reference or URL is invalid.
    InvalidField {
        /// Name of the invalid field.
        field: &'static str,
    },
    /// A required URL parameter appears more than once.
    DuplicateParameter {
        /// Name of the repeated parameter.
        field: &'static str,
    },
}

/// The part of Discord's response that could not be read.
///
/// These details help diagnose an unreadable response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HttpPart {
    /// The first line, which reports the result of the operation.
    StatusLine,
    /// One line of information about the response, such as its size.
    HeaderLine,
    /// A line declaring the size of a piece of the response.
    ChunkSizeLine,
    /// One line of additional information after the response contents.
    TrailerLine,
    /// All additional information after the response contents.
    Trailers,
}

/// Why Discord's response could not be read.
///
/// Returned through [`Error::HttpParseError`]. These details describe the
/// server response rather than the contents of the stored file.
#[derive(Debug)]
#[non_exhaustive]
pub enum HttpError {
    /// The connection ended before a response line was complete.
    UnexpectedEof {
        /// The part of the response being read.
        part: HttpPart,
    },
    /// Text describing the response cannot be read as UTF-8.
    InvalidUtf8 {
        /// The part of the response being read.
        part: HttpPart,
        /// Original UTF-8 decoding error.
        source: Utf8Error,
    },
    /// The server used an unsupported HTTP version. HTTP/1.1 is required.
    UnsupportedHttpVersion,
    /// The first response line has an invalid format.
    MalformedStatusLine,
    /// The server returned an invalid HTTP status code.
    InvalidStatusCode {
        /// Value received from the server.
        value: String,
        /// Underlying error, if the value could not be read as a number.
        source: Option<ParseIntError>,
    },
    /// The server declared an invalid response size in `Content-Length`.
    InvalidContentLength {
        /// Value received from the server.
        value: String,
        /// Original integer parsing error.
        source: ParseIntError,
    },
    /// A line describing the response has an invalid format.
    MalformedHeader,
    /// The server declared different sizes for the same response.
    ConflictingContentLength {
        /// First declared length.
        first: usize,
        /// Conflicting declared length.
        second: usize,
    },
    /// The server gave conflicting instructions for reading the response.
    ConflictingBodyFraming,
    /// The server used an unsupported way to send the response contents.
    UnsupportedTransferEncoding {
        /// Rejected Transfer-Encoding header value.
        value: String,
    },
    /// The server did not specify how to find the end of the response.
    MissingBodyFraming,
    /// The server did not declare the size of the file download.
    MissingContentLength,
    /// The server declared an invalid size for a piece of the response.
    InvalidChunkSize {
        /// Line containing the invalid size.
        value: String,
        /// Underlying error, if the value could not be read as a number.
        source: Option<ParseIntError>,
    },
    /// A piece of the response lacks its required ending.
    InvalidChunkDelimiter,
    /// Additional information after the response contents has an invalid format.
    MalformedTrailer,
    /// A response line has an invalid ending.
    InvalidLineEnding {
        /// The part of the response being read.
        part: HttpPart,
    },
    /// The server response is larger than the allowed limit.
    BodyTooLarge {
        /// Maximum allowed response size in bytes.
        limit: usize,
        /// Total response size declared so far, in bytes.
        ///
        /// Saturates at `usize::MAX` if the announced total overflows.
        size: usize,
    },
    /// The server sent more response bytes than its declared size.
    UnexpectedBodyBytes {
        /// Declared response size in bytes.
        expected: usize,
        /// Number of response bytes already received.
        received: usize,
    },
    /// Information describing the response exceeds its size limit.
    MetadataTooLarge {
        /// The part of the response being read.
        part: HttpPart,
        /// Maximum allowed size in bytes.
        limit: usize,
    },
}

/// Why Discord's reply lacks usable details for a stored file.
#[derive(Debug)]
#[non_exhaustive]
pub enum ResponseError {
    /// The message no longer contains the attachment being renewed.
    AttachmentNotFound {
        /// Identifier of the expected attachment.
        attachment_id: u64,
    },
    /// A required file detail is missing.
    MissingField {
        /// JSON field path, such as `attachments[0].url`.
        field: &'static str,
    },
    /// A required file detail has the wrong type of value.
    InvalidFieldType {
        /// JSON field path.
        field: &'static str,
        /// Expected JSON type.
        expected: &'static str,
        /// Actual value received from the server.
        actual: serde_json::Value,
    },
}

/// Why a write failed before accepting any bytes from that call.
///
/// Writing beyond the file size limit returns [`io::ErrorKind::InvalidInput`].
/// Writing after closing starts returns [`io::ErrorKind::BrokenPipe`].
///
/// To inspect these errors from [`crate::WriteFile`], use [`io::Error::get_ref`]
/// and `downcast_ref::<Error>()`, then match [`Error::WriteError`]. Connection
/// errors keep their original I/O error.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WriteError {
    /// The write would exceed the file size limit.
    PayloadLimitExceeded {
        /// Number of file bytes that can still be written.
        remaining: usize,
        /// Number of bytes supplied by the rejected call.
        attempted: usize,
    },
    /// The file is closing or has already been closed for writing.
    NotWritable,
    /// The total size of the supplied buffers cannot be represented by `usize`.
    SizeOverflow,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidWebhookUrl(error) => write!(f, "invalid webhook URL: {error}"),
            Self::InvalidDiscordFileUrl(error) => write!(f, "invalid attachment URL: {error}"),
            Self::IoError(error) => write!(f, "transport I/O failed: {error}"),
            Self::JsonError(error) => write!(f, "JSON encoding or decoding failed: {error}"),
            Self::HttpParseError(error) => write!(f, "invalid HTTP response: {error}"),
            Self::HttpStatus { status, .. } => {
                write!(f, "Discord returned unexpected HTTP status {status}")
            }
            Self::InvalidResponse(error) => write!(f, "invalid upload response: {error}"),
            Self::WriteError(error) => write!(f, "write rejected: {error}"),
            Self::InvalidRange { start, end } => {
                write!(f, "invalid byte range {start}-{end}: expected start < end")
            }
        }
    }
}

impl fmt::Display for WebhookUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::InvalidPrefix => "expected https://discord.com/api/webhooks/ prefix",
            Self::MissingTokenSeparator => "missing separator between identifier and token",
            Self::EmptyId => "empty webhook identifier",
            Self::EmptyToken => "empty webhook token",
        })
    }
}

impl fmt::Display for DiscordFileUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expired => {
                f.write_str("attachment URL has expired or the system clock is invalid")
            }
            Self::InvalidPrefix => {
                f.write_str("expected https://cdn.discordapp.com/attachments/ prefix")
            }
            Self::MissingField { field } => write!(f, "missing field {field}"),
            Self::InvalidField { field } => write!(f, "invalid field {field}"),
            Self::DuplicateParameter { field } => write!(f, "repeated query parameter {field}"),
        }
    }
}

impl fmt::Display for HttpPart {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::StatusLine => "status line",
            Self::HeaderLine => "header line",
            Self::ChunkSizeLine => "chunk size line",
            Self::TrailerLine => "trailer line",
            Self::Trailers => "trailers",
        })
    }
}

impl fmt::Display for HttpError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingContentLength => f.write_str("download response requires Content-Length"),
            Self::UnexpectedEof { part } => write!(f, "unexpected EOF while reading {part}"),
            Self::InvalidUtf8 { part, source } => write!(f, "invalid UTF-8 in {part}: {source}"),
            Self::UnsupportedHttpVersion => f.write_str("expected HTTP/1.1 response version"),
            Self::MalformedStatusLine => {
                f.write_str("missing separator between status code and reason phrase")
            }
            Self::InvalidStatusCode { value, source } => {
                write!(
                    f,
                    "invalid status code {value:?}: expected three digits between 100 and 599"
                )?;
                if let Some(source) = source {
                    write!(f, ": {source}")?;
                }
                Ok(())
            }
            Self::InvalidContentLength { value, source } => {
                write!(f, "invalid Content-Length {value:?}: {source}")
            }
            Self::MalformedHeader => f.write_str("header line is missing a colon separator"),
            Self::ConflictingContentLength { first, second } => {
                write!(f, "conflicting Content-Length values: {first} and {second}")
            }
            Self::ConflictingBodyFraming => {
                f.write_str("both Content-Length and Transfer-Encoding are present")
            }
            Self::UnsupportedTransferEncoding { value } => {
                write!(f, "unsupported or repeated Transfer-Encoding: {value:?}")
            }
            Self::MissingBodyFraming => {
                f.write_str("missing Content-Length or chunked transfer encoding")
            }
            Self::InvalidChunkSize { value, source } => {
                write!(
                    f,
                    "invalid chunk size line {value:?}: expected at most 16 hexadecimal digits and optional extensions within usize range"
                )?;
                if let Some(source) = source {
                    write!(f, ": {source}")?;
                }
                Ok(())
            }
            Self::InvalidChunkDelimiter => f.write_str("expected CRLF after chunk data"),
            Self::MalformedTrailer => {
                f.write_str("trailer is missing a colon or has an invalid name or value")
            }
            Self::InvalidLineEnding { part } => write!(f, "expected CRLF at the end of {part}"),
            Self::BodyTooLarge { limit, size } => write!(
                f,
                "response body size {size} exceeds the {limit}-byte limit"
            ),
            Self::UnexpectedBodyBytes { expected, received } => write!(
                f,
                "received {received} body bytes for Content-Length {expected}"
            ),
            Self::MetadataTooLarge { part, limit } => {
                write!(f, "{part} exceeds the {limit}-byte limit")
            }
        }
    }
}

impl fmt::Display for ResponseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::AttachmentNotFound { attachment_id } => {
                write!(f, "message does not contain attachment {attachment_id}")
            }
            Self::MissingField { field } => write!(f, "missing field {field}"),
            Self::InvalidFieldType {
                field, expected, ..
            } => write!(f, "field {field} must be a JSON {expected}"),
        }
    }
}

impl fmt::Display for WriteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PayloadLimitExceeded {
                remaining,
                attempted,
            } => write!(
                f,
                "attempted to write {attempted} bytes with {remaining} payload bytes remaining"
            ),
            Self::NotWritable => f.write_str("request body is closing or already closed"),
            Self::SizeOverflow => f.write_str("combined vectored write size overflows usize"),
        }
    }
}

impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        Self::IoError(error)
    }
}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::JsonError(error)
    }
}

impl From<HttpError> for Error {
    fn from(error: HttpError) -> Self {
        Self::HttpParseError(error)
    }
}

impl From<ResponseError> for Error {
    fn from(error: ResponseError) -> Self {
        Self::InvalidResponse(error)
    }
}

impl From<WebhookUrlError> for Error {
    fn from(error: WebhookUrlError) -> Self {
        Self::InvalidWebhookUrl(error)
    }
}

impl From<DiscordFileUrlError> for Error {
    fn from(error: DiscordFileUrlError) -> Self {
        Self::InvalidDiscordFileUrl(error)
    }
}

impl From<WriteError> for Error {
    fn from(error: WriteError) -> Self {
        Self::WriteError(error)
    }
}

impl From<WriteError> for io::Error {
    fn from(error: WriteError) -> Self {
        let kind = match error {
            WriteError::NotWritable => io::ErrorKind::BrokenPipe,
            WriteError::PayloadLimitExceeded { .. } | WriteError::SizeOverflow => {
                io::ErrorKind::InvalidInput
            }
        };
        Self::new(kind, Error::WriteError(error))
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidWebhookUrl(error) => Some(error),
            Self::InvalidDiscordFileUrl(error) => Some(error),
            Self::IoError(error) => Some(error),
            Self::JsonError(error) => Some(error),
            Self::HttpParseError(error) => Some(error),
            Self::InvalidResponse(error) => Some(error),
            Self::WriteError(error) => Some(error),
            Self::HttpStatus { .. } | Self::InvalidRange { .. } => None,
        }
    }
}

impl std::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InvalidUtf8 { source, .. } => Some(source),
            Self::InvalidContentLength { source, .. } => Some(source),
            Self::InvalidStatusCode { source, .. } | Self::InvalidChunkSize { source, .. } => {
                source
                    .as_ref()
                    .map(|source| source as &(dyn std::error::Error + 'static))
            }
            _ => None,
        }
    }
}

impl std::error::Error for WebhookUrlError {}
impl std::error::Error for DiscordFileUrlError {}
impl std::error::Error for ResponseError {}
impl std::error::Error for WriteError {}

/// The result of a file operation or URL check, with [`Error`] on failure.
pub type Result<T> = std::result::Result<T, Error>;
