//! Platform-neutral core of Tarloupe.
//!
//! The pipeline is: codec layer → tar walker → entry table, persisted in a
//! SQLite index (`.tli`) keyed to the archive. Reads go from an entry to a
//! range of the uncompressed stream. For a plain `.tar` that range is a
//! direct file offset; compressed archives (gzip, zstd, xz, bzip2) reach it
//! through decoder restart points (see [`stream`]).

pub mod archive;
mod bz2;
pub mod codec;
pub mod convert;
pub mod entry;
pub mod error;
pub mod gzip;
pub mod header;
pub mod index;
pub mod key;
pub mod listing;
pub mod path;
pub mod pax;
pub mod stream;
pub mod vfs;
pub mod walker;
mod xz;
mod zstd;

pub use archive::{Archive, OpenOptions, Progress};
pub use codec::Codec;
pub use entry::{Entry, EntryKind, SparseChunk};
pub use error::{Error, Result};
pub use walker::{ReadSource, SeekSource, TarSource, Walker};
