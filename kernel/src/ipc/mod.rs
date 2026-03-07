// kernel/src/ipc/mod.rs
//
// Kernel IPC subsystem.
//
// The core primitive is `Channel`: a unidirectional, fixed-capacity message
// queue with 64-byte messages (one L1 cache line each).
//
// A connected socket pair is two Channels with `peer` pointers linking them.
//
// The POSIX-compatible syscall layer (socket/bind/connect/accept/send/recv)
// is implemented in process/syscall.rs on top of this module.

pub mod channel;

pub use channel::{Channel, ChannelId, Message, CHANNELS};
