use std::fmt;

/// A format, resource, or transformation failure. 
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum Error {
   InvalidWebhookUrl,
   HttpError,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::HttpError => "http error",
            Self::InvalidWebhookUrl => "invalid webhook url",
        })
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
