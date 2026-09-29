#![doc = include_str!("../README.md")]

mod credentials;
mod error;
mod write;

pub use credentials::WebhookCredentials;
pub use error::{Error, Result};
pub use write::{WriteConfig, WriteFile};
