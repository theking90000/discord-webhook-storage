use core::str;
use std::fmt;

use crate::Error::{HttpParseError, JsonError};

/// A format, resource, or transformation failure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Error {
    InvalidWebhookUrl,
    IoError,
    JsonError,
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

impl From<httparse::Error> for Error {
    fn from(_: httparse::Error) -> Self {
        HttpParseError
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

pub type Result<T> = std::result::Result<T, Error>;
