//! Shared foundations (L0): paths, storage, settings, ids, logging.

pub mod git;
pub mod logging;
pub mod paths;
pub mod platform;
pub mod redact;
pub mod settings;
pub mod settings_doc;
pub mod storage;

/// New time-sortable id (UUIDv7, L0.8).
pub fn new_id() -> uuid::Uuid {
    uuid::Uuid::now_v7()
}
