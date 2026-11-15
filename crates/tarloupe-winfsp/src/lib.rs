//! Shows a tar archive as a read-only folder or drive on Windows, through
//! [WinFsp](https://winfsp.dev). Names, case-insensitive lookups and links
//! come from [`tarloupe_core::vfs`]; this crate only adapts it to WinFsp.
//!
//! The crate is empty on other platforms.

#[cfg(windows)]
mod fs;

#[cfg(windows)]
pub use fs::{mount, Mount, MountError};
