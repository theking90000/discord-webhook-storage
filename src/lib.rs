#![doc = include_str!("../README.md")]

mod chunk_writer;
mod credentials;
mod error;
mod file;
mod http;
mod read;
mod write;

pub use credentials::WebhookCredentials;
pub use error::{
    DiscordFileUrlError, Error, HttpError, HttpPart, ResponseError, Result, WebhookUrlError,
    WriteError,
};
pub use file::{DiscordFile, DiscordFileUrl};
pub use read::ReadFile;
pub use write::{WriteConfig, WriteFile};
