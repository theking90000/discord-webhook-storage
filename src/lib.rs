#![doc = include_str!("../README.md")]

mod credentials;
mod error;
mod http;
mod write;
mod chunk_writer;

pub use credentials::WebhookCredentials;
pub use error::{Error, Result};
pub use write::{WriteConfig, WriteFile, WrittenFile};
