// kernel/src/block/mod.rs
//
// Block device layer. Currently just the ATA PIO driver — the only thing
// that needs raw sector I/O today is fs::ext2.

pub mod ata;
