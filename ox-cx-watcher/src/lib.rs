//! `ox-cx-watcher` — the reference event source for Ox.
//!
//! A standalone process that observes a cx-enabled repository and
//! posts source events to `ox-server`'s `/api/events/ingest` endpoint.
//! ox-server has no cx-specific code; everything cx-flavored lives in
//! this crate.

pub mod client;
pub mod cx;
pub mod mapping;
