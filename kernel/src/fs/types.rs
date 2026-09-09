// kernel/src/fs/types.rs
//
// Shared VFS types: Stat, Errno, DirEntry, FileType, OpenFlags.
//
// These now live in the standalone, host-testable `vfs` crate
// (`vfs/src/types.rs`, `cd vfs && cargo test`) — see
// `docs/fs/vfs-extraction-plan.md`. This file stays as a re-export so no
// `use crate::fs::types::...` anywhere in the kernel has to change.

pub use vfs::types::{DirEntry, Errno, FileType, OpenFlags, Stat};
