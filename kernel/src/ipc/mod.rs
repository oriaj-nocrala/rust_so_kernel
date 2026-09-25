// kernel/src/ipc/mod.rs
//
// Kernel IPC subsystem: AF_UNIX sockets, and `memfd` (shared memory
// behind an fd, `memfd.rs`).
//
// This used to be `channel.rs`, a bespoke primitive of fixed 64-byte
// messages reached through a `socket()` that took no arguments — no domain,
// no type, no `sockaddr`, no `listen()`. It has been replaced outright by
// real AF_UNIX: the state machines live in the host-tested `usock` crate
// (`cd usock && cargo test`), and `unix.rs` is the kernel adapter that gives
// them a global table, an fd-facing `FileHandle`, and blocking.
//
// The syscall layer (socket/bind/listen/connect/accept/send*/recv*/shutdown/
// getsockname/getpeername/socketpair/set-getsockopt) is in
// `process/syscall/ipc.rs`, on top of this module.

pub mod memfd;
pub mod pty;
pub mod unix;

pub use unix::{UnixSocketHandle, SOCKETS};
