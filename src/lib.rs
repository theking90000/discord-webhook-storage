#![doc = include_str!("../README.md")]

mod chunk_writer;
mod credentials;
mod error;
mod http;
mod write;

pub use credentials::WebhookCredentials;
pub use error::{Error, Result};
pub use write::{WriteConfig, WriteFile, WrittenFile};
