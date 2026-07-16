//! timberfs as a library: the store, the read path, and the entry
//! machinery shared by the `timberfs` and `timbergrep` binaries. The
//! binaries stay thin; the transport between them is the record stream
//! (timberfs-records(5)), not this crate — linking it is code sharing,
//! not the interface.

pub mod append;
pub mod bark;
pub mod entry;
pub mod export;
pub mod forest;
pub mod format;
pub mod fs;
pub mod grain;
pub mod grep;
pub mod import;
pub mod list;
pub mod note;
pub mod query;
pub mod records;
pub mod rotate;
pub mod sink;
pub mod store;
