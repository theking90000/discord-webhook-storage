use core::str;
use std::fmt;

use crate::Error::{HttpParseError, JsonError};

/// A failure while parsing credentials, sending an upload, or reading its response.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Error {
    /// The webhook URL does not have the expected prefix or credential separator.
    InvalidWebhookUrl,
    /// The transport failed to read, write, flush, or finish the request body.
    IoError,
    /// Request serialization or response JSON decoding failed.
    JsonError,
    /// The HTTP response is malformed, oversized, or has an unsuccessful status.
    ///
    /// This also covers missing message identifiers and attachment URLs.
    HttpParseError,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::IoError => "io error",
            Self::InvalidWebhookUrl => "invalid webhook url",
            Self::JsonError => "json error",
            Self::HttpParseError => "http parse error",
        })
    }
}

impl From<std::io::Error> for Error {
    fn from(_: std::io::Error) -> Self {
        Self::IoError
    }
}

impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        JsonError
    }
}

impl From<str::Utf8Error> for Error {
    fn from(_: str::Utf8Error) -> Self {
        HttpParseError
    }
}

impl From<std::num::ParseIntError> for Error {
    fn from(_: std::num::ParseIntError) -> Self {
        HttpParseError
    }
}

impl std::error::Error for Error {}

/// A result returned by webhook credential parsing and upload operations.
pub type Result<T> = std::result::Result<T, Error>;
