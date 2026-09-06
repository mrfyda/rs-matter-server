//! Persistent controller state that is not owned by rs-matter.
//!
//! rs-matter persists the fabric and its certificates; everything else the
//! protocol promises to survive a restart — nodes, interview results,
//! credentials, the node-id counter, the fabric label — lives here.

pub mod config;
pub mod nodes;
pub mod thread_dataset;

pub use config::ConfigStore;
pub use nodes::{InterviewDiff, NodeStore, StoredNode};
