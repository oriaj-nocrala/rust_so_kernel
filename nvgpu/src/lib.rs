//! `nvgpu` — host-testable logic of the NVIDIA GA106 (RTX 3050) driver.
//!
//! The plan is `docs/gpu/gpu-plan.md`. The rules that matter here:
//! - every register offset, bit and structure layout cites
//!   `file:line` of the pinned references (`~/src/gpu-ref/PINNED`: Linux
//!   v7.2.2's nouveau, open-gpu-kernel-modules 570.144);
//! - registers are reached through the [`Mmio`] seam, so sequences run
//!   under `cargo test` against real captured values;
//! - nothing blocks and nothing logs: results come back as data, and the
//!   kernel adapter (`kernel/src/gpu/`) does the logging.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod aux;
pub mod dcb;
pub mod display;
pub mod dp;
pub mod dispstate;
pub mod edid;
pub mod evo;
pub mod hdmi;
pub mod i2c;
pub mod id;
pub mod init;
pub mod lut;
pub mod mmio;
pub mod mode;
pub mod pad;
pub mod pattern;
pub mod pll;
pub mod supervisor;
pub mod vbios;
pub mod vblank;

pub use mmio::Mmio;
