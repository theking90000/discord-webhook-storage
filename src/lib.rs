#![doc = include_str!("../README.md")]

mod chunk_writer;
mod credentials;
mod error;
mod http;
mod write;
mod file;

pub use credentials::WebhookCredentials;
pub use error::{
    DiscordFileUrlError, Error, HttpError, HttpPart, ResponseError, Result, WebhookUrlError, WriteError,
};
pub use write::{WriteConfig, WriteFile};
pub use file::{DiscordFile, DiscordFileUrl};
