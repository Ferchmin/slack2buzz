//! Slack → Buzz archive importer.
//!
//! Two stages, always: `parse` turns a Slack export into [`ir`] records on
//! disk, and `emit` turns those records into signed Buzz events. They share no
//! state beyond the IR file, which is what lets either be re-run alone.

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
