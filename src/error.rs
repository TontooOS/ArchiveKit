//! Error types for ArchiveKit.

use std::fmt;

/// Main error type for ArchiveKit operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArchiveError {
    /// Invalid input data (bad magic, truncated, corrupt).
    InvalidData(String),
    /// Unsupported feature (e.g. encrypted ZIP, unsupported method).
    Unsupported(String),
    /// Checksum mismatch (CRC32).
    ChecksumMismatch {
        expected: u32,
        actual: u32,
        entry: String,
    },
    /// I/O error with a message (kept as string to stay dependency-free).
    Io(String),
    /// Requested entry was not found.
    NotFound(String),
    /// A path inside the archive is unsafe (absolute or escapes root).
    UnsafePath(String),
}

impl fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArchiveError::InvalidData(msg) => write!(f, "invalid data: {msg}"),
            ArchiveError::Unsupported(msg) => write!(f, "unsupported: {msg}"),
            ArchiveError::ChecksumMismatch {
                expected,
                actual,
                entry,
            } => write!(
                f,
                "checksum mismatch in '{entry}': expected {expected:08x}, got {actual:08x}"
            ),
            ArchiveError::Io(msg) => write!(f, "i/o error: {msg}"),
            ArchiveError::NotFound(msg) => write!(f, "not found: {msg}"),
            ArchiveError::UnsafePath(msg) => write!(f, "unsafe path: {msg}"),
        }
    }
}

impl std::error::Error for ArchiveError {}

impl From<std::io::Error> for ArchiveError {
    fn from(e: std::io::Error) -> Self {
        ArchiveError::Io(e.to_string())
    }
}

/// Common result type for ArchiveKit.
pub type Result<T> = std::result::Result<T, ArchiveError>;

/// Convert a thread-local style message helper.
pub(crate) fn invalid(msg: impl Into<String>) -> ArchiveError {
    ArchiveError::InvalidData(msg.into())
}

pub(crate) fn unsupported(msg: impl Into<String>) -> ArchiveError {
    ArchiveError::Unsupported(msg.into())
}
