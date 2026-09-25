//! The compositor's logic, apart from the machine it runs on (phase 2.4 of
//! `docs/gui/gui-plan.md`).
//!
//! Same shape as `usock`: **nothing blocks and every effect comes back as
//! data** — events to send, rectangles to flush, clients to drop. The
//! program that owns the sockets, the screen and `epoll` (`compositor` in
//! the `userspace` crate, phase 2.5) feeds bytes and input in and carries
//! the results out, so everything here runs under a plain `cargo test`,
//! composing into a `Vec<u32>` and checking pixels.
//!
//! - [`region`]: rectangles and disjoint rectangle sets — damage, clipping,
//!   what a window covers.
//! - [`wire`]: Wayland's wire format, unchanged — framing, argument
//!   encoding, messages split across reads, fds carried out of band.
//! - [`protocol`]: this compositor's own minimal protocol, with Wayland's
//!   names, as typed requests and events over `wire`.
//! - [`compositor`]: clients, their objects, surfaces with pending and
//!   current state, stacking, focus, the pointer, and `compose`.

#![no_std]

extern crate alloc;

pub mod compositor;
pub mod protocol;
pub mod region;
pub mod wire;
