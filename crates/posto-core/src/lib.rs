//! Posto core: config, secrets, SQLite store, JMAP sync, compose, theme, shortcuts, UI fixtures.

pub mod actions;
pub mod body;
pub mod compose;
pub mod config;
pub mod error;
pub mod fixtures;
pub mod html;
pub mod jmap;
pub mod shortcuts;
pub mod store;
pub mod sync;
pub mod theme;

pub use error::{Error, Result};
