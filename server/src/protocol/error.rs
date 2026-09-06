//! Protocol error codes.
//!
//! The numeric values are part of the wire contract: they match the Python
//! Matter Server codes that matterjs-server reuses, plus the two Open Home
//! Foundation extensions (100, 101). Clients switch on the number, so the
//! discriminants must never be reordered.

use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i64)]
pub enum ErrorCode {
    UnknownError = 0,
    NodeCommissionFailed = 1,
    NodeInterviewFailed = 2,
    NodeNotReady = 3,
    NodeNotResolving = 4,
    NodeNotExists = 5,
    VersionMismatch = 6,
    SdkStackError = 7,
    InvalidArguments = 8,
    InvalidCommand = 9,
    UpdateCheckError = 10,
    UpdateError = 11,
    /// OHF extension: ICD registration rejected because other-vendor
    /// administrators may not support LIT.
    IcdMultiAdmin = 100,
    /// OHF extension: OTA image upload failed.
    OtaUploadError = 101,
}

impl ErrorCode {
    pub fn as_i64(self) -> i64 {
        self as i64
    }
}

/// An error response: a code plus a human-readable detail string.
#[derive(Clone, Debug)]
pub struct ApiError {
    pub code: ErrorCode,
    pub details: String,
}

impl ApiError {
    pub fn new(code: ErrorCode, details: impl Into<String>) -> Self {
        Self {
            code,
            details: details.into(),
        }
    }

    pub fn unknown(details: impl Into<String>) -> Self {
        Self::new(ErrorCode::UnknownError, details)
    }

    pub fn commission_failed(details: impl Into<String>) -> Self {
        Self::new(ErrorCode::NodeCommissionFailed, details)
    }

    pub fn interview_failed(details: impl Into<String>) -> Self {
        Self::new(ErrorCode::NodeInterviewFailed, details)
    }

    pub fn node_not_ready(node_id: u64) -> Self {
        Self::new(
            ErrorCode::NodeNotReady,
            format!("Node {} is not ready", node_id),
        )
    }

    pub fn node_not_resolving(node_id: u64) -> Self {
        Self::new(
            ErrorCode::NodeNotResolving,
            format!("Node {} is not resolving", node_id),
        )
    }

    pub fn node_not_exists(node_id: u64) -> Self {
        Self::new(
            ErrorCode::NodeNotExists,
            format!("Node {} does not exist", node_id),
        )
    }

    pub fn sdk(details: impl Into<String>) -> Self {
        Self::new(ErrorCode::SdkStackError, details)
    }

    pub fn invalid_args(details: impl Into<String>) -> Self {
        Self::new(ErrorCode::InvalidArguments, details)
    }

    pub fn invalid_command(command: &str) -> Self {
        Self::new(
            ErrorCode::InvalidCommand,
            format!("Unknown command: {}", command),
        )
    }

    pub fn update_check(details: impl Into<String>) -> Self {
        Self::new(ErrorCode::UpdateCheckError, details)
    }

    pub fn update(details: impl Into<String>) -> Self {
        Self::new(ErrorCode::UpdateError, details)
    }

    pub fn ota_upload(details: impl Into<String>) -> Self {
        Self::new(ErrorCode::OtaUploadError, details)
    }

    /// The ICD multi-admin error carries a JSON document as its `details`
    /// string, so clients can recover the offending vendor list.
    pub fn icd_multi_admin(admin_vendor_ids: &[u16]) -> Self {
        let details = serde_json::json!({
            "message": "Peer has administrators from other vendors that may not support LIT",
            "admin_vendor_ids": admin_vendor_ids,
        });
        Self::new(ErrorCode::IcdMultiAdmin, details.to_string())
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}] {}", self.code.as_i64(), self.details)
    }
}

impl std::error::Error for ApiError {}

/// Every command handler returns this.
pub type ApiResult = std::result::Result<serde_json::Value, ApiError>;
