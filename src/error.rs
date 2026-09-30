use std::{fmt, io, num::ParseIntError, str::Utf8Error};

/// A failure while parsing URLs, uploading, or downloading a file.
///
/// Transport and JSON errors retain their original causes through
/// [`std::error::Error::source`]. HTTP syntax, unsuccessful statuses, invalid
/// response fields, and rejected writes have separate variants.
#[derive(Debug)]
#[non_exhaustive]
pub enum Error {
    /// The webhook URL cannot provide an identifier and token.
    InvalidWebhookUrl(WebhookUrlError),
    /// The attachment URL cannot provide the required structured fields.
    InvalidDiscordFileUrl(DiscordFileUrlError),
    /// The transport failed, retaining its error kind and original cause.
    IoError(io::Error),
    /// Request serialization or response JSON decoding failed.
    JsonError(serde_json::Error),
    /// The HTTP response cannot be parsed or decoded within its limits.
    HttpParseError(HttpError),
    /// Discord returned a status that the operation cannot accept.
    HttpStatus {
        /// HTTP status code returned by Discord.
        status: u16,
        /// Decoded response body, bounded by the response size limit.
        ///
        /// Kept as bytes because unsuccessful responses need not contain JSON
        /// or valid UTF-8. Parse it with Serde to inspect Discord error details.
        body: Vec<u8>,
    },
    /// Valid JSON does not contain the required upload result fields.
    InvalidResponse(ResponseError),
    /// A write was rejected before accepting any bytes from that call.
    WriteError(WriteError),
    /// The requested byte range does not have `start < end`.
    InvalidRange {
        /// First requested byte offset.
        start: usize,
        /// Inclusive last requested byte offset.
        end: usize,
    },
}

/// The reason a webhook URL was rejected, without retaining its secret token.
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

/// The reason an attachment URL was rejected, without retaining its signature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum DiscordFileUrlError {
    /// The signed attachment URL has expired or the system clock is invalid.
    Expired,
    /// The URL does not start with `https://cdn.discordapp.com/attachments/`.
    InvalidPrefix,
    /// A required path segment or query parameter is absent.
    MissingField {
        /// Name of the missing field.
        field: &'static str,
    },
    /// A field is empty, malformed, or outside the range of `u64`.
    InvalidField {
        /// Name of the invalid field.
        field: &'static str,
    },
    /// A signing query parameter occurs more than once.
    DuplicateParameter {
        /// Name of the repeated parameter.
        field: &'static str,
    },
}

/// The part of an HTTP response involved in a shared parsing failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum HttpPart {
    /// The HTTP version, status code, and reason phrase.
    StatusLine,
    /// A response header line.
    HeaderLine,
    /// A chunk size and its optional extensions.
    ChunkSizeLine,
    /// A single trailer header line.
    TrailerLine,
    /// The complete trailer section.
    Trailers,
}

/// A failure in HTTP response syntax, framing, or size limits.
#[derive(Debug)]
#[non_exhaustive]
pub enum HttpError {
    /// A line ended prematurely because the connection reached EOF.
    UnexpectedEof {
        /// The part of the response being read.
        part: HttpPart,
    },
    /// Response metadata contains invalid UTF-8.
    InvalidUtf8 {
        /// The part of the response being decoded.
        part: HttpPart,
        /// Original UTF-8 decoding error.
        source: Utf8Error,
    },
    /// The status line does not start with the supported `HTTP/1.1` version.
    UnsupportedHttpVersion,
    /// The status line lacks a separator between the code and reason phrase.
    MalformedStatusLine,
    /// The status code is not three digits between 100 and 599.
    InvalidStatusCode {
        /// Value received from the server.
        value: String,
        /// Integer parsing error, if parsing failed before validating the code.
        source: Option<ParseIntError>,
    },
    /// A Content-Length value could not be parsed as a byte count.
    InvalidContentLength {
        /// Value received from the server.
        value: String,
        /// Original integer parsing error.
        source: ParseIntError,
    },
    /// A header line lacks a colon separator.
    MalformedHeader,
    /// Multiple Content-Length headers disagree.
    ConflictingContentLength {
        /// First declared length.
        first: usize,
        /// Conflicting declared length.
        second: usize,
    },
    /// Both Content-Length and Transfer-Encoding were specified.
    ConflictingBodyFraming,
    /// A transfer encoding other than a single `chunked` header was received.
    UnsupportedTransferEncoding {
        /// Rejected Transfer-Encoding header value.
        value: String,
    },
    /// Neither Content-Length nor chunked transfer encoding was specified.
    MissingBodyFraming,
    /// A streaming download does not declare Content-Length.
    MissingContentLength,
    /// The chunk size has invalid syntax or cannot fit in `usize`.
    InvalidChunkSize {
        /// Rejected chunk size line, including any extensions.
        value: String,
        /// Integer parsing error, if the syntax was valid but parsing failed.
        source: Option<ParseIntError>,
    },
    /// Chunk data is not followed by a CRLF delimiter.
    InvalidChunkDelimiter,
    /// A trailer lacks a colon separator or has an invalid name or value.
    MalformedTrailer,
    /// A chunk size or trailer line does not end with CRLF.
    InvalidLineEnding {
        /// The part of the response being read.
        part: HttpPart,
    },
    /// The decoded response body would exceed its limit.
    BodyTooLarge {
        /// Maximum decoded body length in bytes.
        limit: usize,
        /// Announced total decoded length in bytes.
        ///
        /// Saturates at `usize::MAX` if the announced total overflows.
        size: usize,
    },
    /// Bytes already read exceed the declared Content-Length.
    UnexpectedBodyBytes {
        /// Declared body length in bytes.
        expected: usize,
        /// Number of body bytes already received.
        received: usize,
    },
    /// Chunk framing or trailers exceed their metadata limit.
    MetadataTooLarge {
        /// The metadata being read.
        part: HttpPart,
        /// Maximum allowed size in bytes.
        limit: usize,
    },
}

/// A missing or incorrectly typed field in a successful JSON response.
#[derive(Debug)]
#[non_exhaustive]
pub enum ResponseError {
    /// A required field or array element is absent.
    MissingField {
        /// JSON field path, such as `attachments[0].url`.
        field: &'static str,
    },
    /// A required field has an unexpected JSON type.
    InvalidFieldType {
        /// JSON field path.
        field: &'static str,
        /// Expected JSON type.
        expected: &'static str,
        /// Actual value received from the server.
        actual: serde_json::Value,
    },
}

/// A local write rejection, also available inside an [`io::Error`].
///
/// [`crate::WriteFile`] embeds [`Error::WriteError`] in the errors returned by
/// [`futures::AsyncWrite`]. Use [`io::Error::get_ref`] and `downcast_ref::<Error>()`
/// to inspect the rejection. Transport errors pass through unchanged.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum WriteError {
    /// The write exceeds the remaining file payload allowance.
    PayloadLimitExceeded {
        /// Number of payload bytes still available.
        remaining: usize,
        /// Number of bytes supplied by the rejected call.
        attempted: usize,
    },
    /// Closing has started, or the request body is already complete.
    NotWritable,
    /// The combined size of vectored buffers overflows `usize`.
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

/// A result returned by URL parsing, upload, and download operations.
pub type Result<T> = std::result::Result<T, Error>;
