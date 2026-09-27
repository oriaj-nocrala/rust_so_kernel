// kernel/src/firmware.rs
//
// Device firmware, read from `/mnt/lib/firmware/<path>` (phase 1 of
// docs/gpu/gpu-plan.md, decision D3). Firmware never enters git: the root
// build.rs copies it from the host's `/usr/lib/firmware`, decompressed,
// into `disk-image-root/lib/firmware/`, from where it reaches `disk.img`
// and the stick. So it exists only after `fs::init` has mounted `/mnt`.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::fs::types::OpenFlags;

pub const ROOT: &str = "/mnt/lib/firmware";

/// Largest file `load` accepts: the biggest one this kernel is planned to
/// load is `gsp-570.144.bin`, 63 571 696 bytes (GPU plan, D1).
const MAX_LEN: usize = 96 << 20;

#[derive(Debug)]
pub enum FwError {
    NotFound(String),
    TooBig(usize),
    Io(&'static str),
}

/// The whole of `ROOT/<rel>`, e.g. `nvidia/ga106/gsp/bootloader-570.144.bin`.
pub fn load(rel: &str) -> Result<Vec<u8>, FwError> {
    let path = format!("{}/{}", ROOT, rel);
    let inode = crate::fs::vfs::resolve(&path).map_err(|_| FwError::NotFound(path.clone()))?;
    let size = inode.stat().st_size.max(0) as usize;
    if size > MAX_LEN {
        return Err(FwError::TooBig(size));
    }
    drop(inode);
    let mut fh = crate::fs::vfs::open(&path, OpenFlags::RDONLY).map_err(|_| FwError::NotFound(path))?;
    let mut data = Vec::new();
    data.try_reserve_exact(size).map_err(|_| FwError::Io("out of memory"))?;
    data.resize(size, 0);
    let mut done = 0;
    while done < size {
        match fh.read(&mut data[done..]) {
            Ok(0) | Err(vfs::file::FileError::EndOfFile) => break,
            Ok(n) => done += n,
            Err(_) => return Err(FwError::Io("read failed")),
        }
    }
    if done != size {
        return Err(FwError::Io("short read"));
    }
    Ok(data)
}
