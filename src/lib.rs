#![deny(unsafe_code)]

//! Slack → Buzz archive importer.
//!
//! Two stages, always: `parse` turns a Slack export into [`ir`] records on
//! disk, and `emit` turns those records into signed Buzz events. They share no
//! state beyond the IR file, which is what lets either be re-run alone.
//!
//! Errors follow Buzz's convention: this library returns the `thiserror`-based
//! [`Error`], and `anyhow` appears only in `main.rs`. A single crate cannot have
//! Cargo enforce that split the way Buzz's separate crates do, so it is a
//! convention here — do not reach for `anyhow` in a module.

pub mod error;
pub mod export;
pub mod fmt;
pub mod invite;
pub mod ir;
pub mod ledger;
pub mod mrkdwn;
pub mod parse;
pub mod probe;
pub mod selection;
pub mod slack;

pub use error::{Error, Result};
