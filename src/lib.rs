//! timberfs as a library: the store, the read path, and the entry
//! machinery shared by the `timberfs` and `timbergrep` binaries. The
//! binaries stay thin; the transport between them is the record stream
//! (timberfs-records(5)), not this crate — linking it is code sharing,
//! not the interface.

pub mod append;
pub mod bark;
pub mod cursor;
pub mod entry;
pub mod export;
pub mod follow;
pub mod follower;
pub mod forest;
pub mod format;
pub mod forward;
pub mod frame;
pub mod frames;
pub mod fs;
pub mod grain;
pub mod grep;
pub mod import;
pub mod incus;
pub mod incus_intake;
pub mod intake;
pub mod list;
pub mod live;
pub mod note;
pub mod otlp;
pub mod otlp_intake;
pub mod protobuf;
pub mod query;
pub mod querydoc;
pub mod receive;
pub mod records;
pub mod rotate;
pub mod sap;
pub mod select;
pub mod serve;
pub mod sink;
pub mod store;
pub mod store_json;
