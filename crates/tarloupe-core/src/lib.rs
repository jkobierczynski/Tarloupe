//! Platform-neutral core of Tarloupe.
//!
//! The pipeline is: codec layer → tar walker → entry table, persisted in a
//! SQLite index (`.tli`) keyed to the archive. Reads go from an entry to a
//! range of the uncompressed stream. M0 handles uncompressed `.tar`, where
//! that range is a direct file offset; later milestones add checkpointed
//! codecs (gzip, zstd, xz, bzip2) behind the same API.

pub mod archive;
pub mod codec;
pub mod entry;
pub mod error;
pub mod gzip;
pub mod header;
pub mod index;
pub mod key;
pub mod listing;
pub mod path;
pub mod pax;
pub mod walker;

pub use archive::{Archive, OpenOptions};
pub use codec::Codec;
pub use entry::{Entry, EntryKind, SparseChunk};
pub use error::{Error, Result};
pub use walker::{ReadSource, SeekSource, TarSource, Walker};
