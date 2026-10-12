//! Posto core: config, secrets, SQLite store and UI fixtures.

pub mod actions;
pub mod body;
pub mod config;
pub mod error;
pub mod fixtures;
pub mod jmap;
pub mod store;
pub mod sync;

pub use error::{Error, Result};
