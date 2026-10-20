//! rapidfind core: an Everything-style instant filename index for NTFS.
//!
//! * [`ntfs`] — raw $MFT scan, FSCTL_ENUM_USN_DATA fallback, USN journal.
//! * [`index`] — compact in-memory index with folded-name arena.
//! * [`query`] / [`search`] — Everything-like query language and parallel search.
//! * [`live`] — applying journal records to the index.
//! * [`persist`] — instant startup from a saved index + journal cursor.
//! * [`engine`] — multi-volume lifecycle and watcher threads.
//! * [`ipc`] — named-pipe protocol between the elevated service and clients.

pub mod config;
pub mod content;
pub mod date;
pub mod engine;
pub mod fmt;
pub mod fold;
pub mod folder;
pub mod history;
pub mod ifilter;
pub mod index;
pub mod ipc;
pub mod live;
pub mod ntfs;
pub mod persist;
pub mod query;
pub mod runs;
pub mod search;
pub mod tree;
