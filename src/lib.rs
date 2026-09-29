#![doc = include_str!("../README.md")]

mod error;
mod credentials;

pub use error::{Error, Result};
pub use credentials::{WebhookCredentials};