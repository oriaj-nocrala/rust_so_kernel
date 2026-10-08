//! What `files` and `files-preview` share, and what can be tested without a screen.
//!
//! - [`entry`]: a folder's entries, their order, types, sizes, dates and permissions as shown, and the folder's preview layout.
//! - [`preview`]: the provider's answer (an image, a text excerpt, or why not), its encoding in the output memfd, and the checks the app
//!   runs on it — the provider decodes untrusted files and is itself untrusted (P6.4).
//! - [`open_with`]: the table from a file's extension to the app that opens it (`/mnt/etc/gui/open`).
//! - [`program`]: whether a file is a program (an `x` bit and an ELF header or a `#!` line), which Enter runs.

pub mod entry;
pub mod open_with;
pub mod preview;
pub mod program;
