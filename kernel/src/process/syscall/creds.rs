// kernel/src/process/syscall/creds.rs
//
// getuid/setuid/setresuid/… and getgroups/setgroups (Linux numbers 102-125). The rules are `process::creds`; this file moves the
// ids in and out of user memory. The snapshot → change → commit split keeps the scheduler lock away from user-memory access.

use super::{errno, validate_user_buffer, with_current_process, SyscallResult};
use crate::process::creds::{Creds, NotPermitted, NGROUPS_MAX};

fn snapshot() -> Creds {
    let mut out = Creds::default();
    with_current_process(|p| { out = p.creds.clone(); 0 });
    out
}

/// Store `new` as the caller's credentials, or return `EPERM`.
fn commit(new: Result<Creds, NotPermitted>) -> SyscallResult {
    match new {
        Ok(c) => { with_current_process(|p| { p.creds = c; 0 }) }
        Err(NotPermitted) => errno::EPERM,
    }
}

pub(super) fn sys_getuid() -> SyscallResult { snapshot().ruid as SyscallResult }
pub(super) fn sys_geteuid() -> SyscallResult { snapshot().euid as SyscallResult }
pub(super) fn sys_getgid() -> SyscallResult { snapshot().rgid as SyscallResult }
pub(super) fn sys_getegid() -> SyscallResult { snapshot().egid as SyscallResult }

pub(super) fn sys_setuid(uid: u32) -> SyscallResult { commit(snapshot().setuid(uid)) }
pub(super) fn sys_setgid(gid: u32) -> SyscallResult { commit(snapshot().setgid(gid)) }
pub(super) fn sys_setreuid(r: u32, e: u32) -> SyscallResult { commit(snapshot().setreuid(r, e)) }
pub(super) fn sys_setregid(r: u32, e: u32) -> SyscallResult { commit(snapshot().setregid(r, e)) }
pub(super) fn sys_setresuid(r: u32, e: u32, s: u32) -> SyscallResult { commit(snapshot().setresuid(r, e, s)) }
pub(super) fn sys_setresgid(r: u32, e: u32, s: u32) -> SyscallResult { commit(snapshot().setresgid(r, e, s)) }

/// umask(95): sets the mask to `mask & 0777` and returns the previous one; never fails.
pub(super) fn sys_umask(mask: u32) -> SyscallResult {
    let mut old = 0;
    with_current_process(|p| {
        old = p.creds.umask;
        p.creds.umask = mask & 0o777;
        0
    });
    old as SyscallResult
}

/// setfsuid/setfsgid return the previous value whether or not the change was allowed.
pub(super) fn sys_setfsuid(uid: u32) -> SyscallResult {
    let old = snapshot();
    let previous = old.fsuid;
    commit(Ok(old.setfsuid(uid)));
    previous as SyscallResult
}

pub(super) fn sys_setfsgid(gid: u32) -> SyscallResult {
    let old = snapshot();
    let previous = old.fsgid;
    commit(Ok(old.setfsgid(gid)));
    previous as SyscallResult
}

/// getresuid/getresgid: three `u32` pointers.
pub(super) fn sys_getresuid(r: usize, e: usize, s: usize) -> SyscallResult {
    let c = snapshot();
    put_ids([r, e, s], [c.ruid, c.euid, c.suid])
}

pub(super) fn sys_getresgid(r: usize, e: usize, s: usize) -> SyscallResult {
    let c = snapshot();
    put_ids([r, e, s], [c.rgid, c.egid, c.sgid])
}

fn put_ids(ptrs: [usize; 3], ids: [u32; 3]) -> SyscallResult {
    for p in ptrs {
        if validate_user_buffer(p as u64, 4).is_err() { return errno::EFAULT; }
    }
    for (p, id) in ptrs.into_iter().zip(ids) {
        unsafe { core::ptr::write_unaligned(p as *mut u32, id); }
    }
    0
}

/// getgroups(size, list): with `size == 0`, only the count.
pub(super) fn sys_getgroups(size: i32, list: usize) -> SyscallResult {
    if size < 0 { return errno::EINVAL; }
    let groups = snapshot().groups;
    if size == 0 { return groups.len() as SyscallResult; }
    if (size as usize) < groups.len() { return errno::EINVAL; }
    if validate_user_buffer(list as u64, groups.len() * 4).is_err() { return errno::EFAULT; }
    for (i, g) in groups.iter().enumerate() {
        unsafe { core::ptr::write_unaligned((list + i * 4) as *mut u32, *g); }
    }
    groups.len() as SyscallResult
}

pub(super) fn sys_setgroups(size: usize, list: usize) -> SyscallResult {
    if size > NGROUPS_MAX { return errno::EINVAL; }
    if size > 0 && validate_user_buffer(list as u64, size * 4).is_err() { return errno::EFAULT; }
    let mut groups = alloc::vec::Vec::with_capacity(size);
    for i in 0..size {
        groups.push(unsafe { core::ptr::read_unaligned((list + i * 4) as *const u32) });
    }
    commit(snapshot().setgroups(groups))
}
