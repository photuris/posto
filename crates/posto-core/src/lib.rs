//! Posto core: config, secrets, SQLite store and UI fixtures.

pub mod compose;
pub mod config;
pub mod error;
pub mod fixtures;
pub mod html;
pub mod shortcuts;
pub mod store;
pub mod theme;

pub use error::{Error, Result};
