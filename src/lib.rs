#![doc = include_str!("../README.md")]

mod credentials;
mod error;
mod write;
mod http;

pub use credentials::WebhookCredentials;
pub use error::{Error, Result};
pub use write::{WriteConfig, WriteFile};
