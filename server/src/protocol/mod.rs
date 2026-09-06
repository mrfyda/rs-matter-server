//! The matterjs-server WebSocket protocol: envelopes, wire models, events.
//!
//! This module is deliberately free of Matter and transport concerns. It
//! defines what goes on the wire and nothing about how it is produced, which
//! is what lets the command handlers be tested without a radio or a socket.

pub mod error;
pub mod events;
pub mod message;
pub mod model;
pub mod paths;

pub use error::{ApiError, ApiResult, ErrorCode};
pub use events::{Event, EventClass, EventSubscriptions};
pub use message::{response_envelope, Args, Request};
pub use model::{MIN_SUPPORTED_SCHEMA_VERSION, SCHEMA_VERSION, TEST_NODE_START};
