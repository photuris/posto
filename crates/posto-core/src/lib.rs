//! Posto core: config, secrets, SQLite store and UI fixtures.

pub mod config;
pub mod error;
pub mod fixtures;
pub mod store;

pub use error::{Error, Result};
