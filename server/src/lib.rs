//! rs-matter-server: A Rust Matter controller server with a matterjs-server-
//! compatible WebSocket API.
//!
//! Module layout:
//! - `protocol` — the wire contract: envelopes, models, events, paths
//! - `api`      — command handlers, one module per area of the protocol
//! - `matter`   — rs-matter: the controller actor, commissioning, TLV codec
//! - `storage`  — persistent state the protocol promises to survive a restart
//! - `ws`       — the listener, connection lifecycle, and HTTP endpoints

pub mod api;
pub mod matter;
pub mod monitor;
pub mod protocol;
pub mod storage;
pub mod ws;
