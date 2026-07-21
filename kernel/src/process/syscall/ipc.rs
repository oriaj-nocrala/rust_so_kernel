// kernel/src/process/syscall/ipc.rs
//

// ============================================================================
// IPC SYSCALLS: socket / bind / connect / accept / sendmsg / recvmsg
// ============================================================================
//
// Each process stores its open channel FDs in its FileDescriptorTable using
// a thin SocketHandle wrapper that implements FileHandle.  The actual Channel
// state lives in ipc::CHANNELS (global table, protected by its own Mutex).
//
// LOCKING ORDER (must never be inverted):
//   cli → SCHEDULER → CHANNELS
//
// The ISR path only touches the SCHEDULER, not CHANNELS, so this is safe.


use spin::Mutex;
use core::sync::atomic::Ordering;
use crate::serial_println;
use crate::process::TrapFrame;
use super::{errno, SyscallResult, validate_user_buffer, CURRENT_SYSCALL_TF};

use crate::ipc::channel::{ChannelId, Message as IpcMessage, ServerState, CHANNELS};

// ============================================================================
// IPC BLOCKING WAITERS
// ============================================================================
//
// Pattern: same as STDIN_WAITER / WAIT_WAITER.
//   1. Syscall saves waiter info (pid + data pointers) in a global slot.
//   2. Syscall calls block_current + jump_to_trapframe → never returns here.
//   3. Wakeup code (from another process's syscall) writes the result directly
//      into the blocked process's trapframe.rax (and into the user buffer when
//      needed, via physical address translation).
//   4. sched.wake() makes the process runnable; it returns from the syscall
//      via iretq with rax already set to the correct value.

struct AcceptWaiter {
    pid:               usize,
    server_channel_id: ChannelId,
}
static ACCEPT_WAITER: Mutex<Option<AcceptWaiter>> = Mutex::new(None);

struct RecvWaiter {
    pid:        usize,
    channel_id: ChannelId,
    /// Physical address of the 64-byte Message buffer (pre-translated at
    /// block time so that delivery in sys_sendmsg skips the page-table walk).
    phys_buf:   u64,
}
static RECV_WAITER: Mutex<Option<RecvWaiter>> = Mutex::new(None);

// ——— SocketHandle — FileHandle wrapper around a channel ————————————————————

/// A FileHandle wrapper around a ChannelId.
/// `read`  ↔  recvmsg (returns one message; blocks if empty)
/// `write` ↔  sendmsg (sends one message to the peer)
///
/// Blocking inside read/write is not used here — callers use
/// sys_recvmsg / sys_sendmsg directly.  The handle is only kept in the FD
/// table so that close() frees the channel.
struct SocketHandle {
    channel_id: ChannelId,
}

impl crate::process::file::FileHandle for SocketHandle {
    fn read(&mut self, buf: &mut [u8]) -> crate::process::file::FileResult<usize> {
        let msg = CHANNELS.lock().get_mut(self.channel_id)
            .and_then(|ch| ch.dequeue());
        match msg {
            Some(m) => {
                let n = core::cmp::min(buf.len(), m.len as usize);
                buf[..n].copy_from_slice(&m.data[..n]);
                Ok(n)
            }
            None => Ok(0),   // non-blocking: no data yet
        }
    }

    fn write(&mut self, buf: &[u8]) -> crate::process::file::FileResult<usize> {
        let msg = IpcMessage::new(0, buf);
        let ok = {
            let mut tbl = CHANNELS.lock();
            let peer_id = tbl.get(self.channel_id).and_then(|ch| ch.peer);
            if let Some(pid) = peer_id {
                tbl.get_mut(pid).map(|ch| ch.enqueue(msg)).unwrap_or(false)
            } else {
                false
            }
        };
        if ok { Ok(buf.len()) } else { Err(crate::process::file::FileError::IOError) }
    }

    fn close(&mut self) -> crate::process::file::FileResult<()> {
        CHANNELS.lock().free(self.channel_id);
        Ok(())
    }

    fn name(&self) -> &str { "socket" }
}

// ——— fd → channel_id side table ———————————————————————————————————————————
//
// FileHandle is a trait object; we can't downcast to SocketHandle in no_std
// (no Any).  Solution: maintain a per-process fd→channel_id side table here.
//
// The challenge: FileHandle is a trait object; we can't downcast to SocketHandle
// in no_std (no Any).  Solution: maintain a per-process fd→channel_id side table
// in the IPC layer rather than in the FileDescriptorTable.
//
// We use a global array indexed by (pid * MAX_FILES + fd).

pub(super) const MAX_PROCS: usize = 32;
pub(super) const MAX_FILES_PER_PROC: usize = 16;

/// fd → channel_id mapping.  0 means "not a socket fd".
pub(super) static FD_CHANNEL_MAP: Mutex<[[ChannelId; MAX_FILES_PER_PROC]; MAX_PROCS]> =
    Mutex::new([[0usize; MAX_FILES_PER_PROC]; MAX_PROCS]);

fn set_fd_channel(pid: usize, fd: usize, channel_id: ChannelId) {
    if pid < MAX_PROCS && fd < MAX_FILES_PER_PROC {
        FD_CHANNEL_MAP.lock()[pid][fd] = channel_id;
    }
}

fn get_fd_channel(pid: usize, fd: usize) -> Option<ChannelId> {
    if pid < MAX_PROCS && fd < MAX_FILES_PER_PROC {
        let id = FD_CHANNEL_MAP.lock()[pid][fd];
        if id != 0 { Some(id) } else { None }
    } else {
        None
    }
}

fn clear_fd_channel(pid: usize, fd: usize) {
    if pid < MAX_PROCS && fd < MAX_FILES_PER_PROC {
        FD_CHANNEL_MAP.lock()[pid][fd] = 0;
    }
}

// ——— sys_socket (revised) — store mapping ——————————————————————————————————

/// Internal helper: open a socket and record the fd→channel mapping.
pub(super) fn sys_socket_impl() -> SyscallResult {
    let pid_dbg = crate::process::scheduler::current_pid().unwrap_or(0);
    serial_println!("[DBG] sys_socket PID {}", pid_dbg);
    let id = match CHANNELS.lock().alloc() {
        Some(id) => id,
        None => return errno::ENOMEM,
    };

    let handle = alloc::boxed::Box::new(SocketHandle { channel_id: id });

    let _irq = crate::process::irq_guard::InterruptGuard::new();
    let mut sched = crate::process::scheduler::local_scheduler();
    match sched.running_mut() {
        Some(proc) => {
            let pid = proc.pid.0;
            // `.lock()`'s guard must not outlive this statement — it
            // borrows (transitively) from `sched`, which the Ok arm
            // below drops, so the Result is computed and the guard
            // dropped first via a `let`, not a bare match scrutinee
            // (which would extend the guard's lifetime across all arms).
            let alloc_result = proc.files.lock().allocate(handle);
            match alloc_result {
                Ok(fd) => {
                    drop(sched);
                    set_fd_channel(pid, fd, id);
                    fd as i64
                }
                Err(_) => {
                    CHANNELS.lock().free(id);
                    errno::EINVAL
                }
            }
        }
        None => {
            CHANNELS.lock().free(id);
            errno::ESRCH
        }
    }
}

// ——— sys_bind (proper implementation) ————————————————————————————————————

pub(super) fn sys_bind_impl(fd: i32, path_ptr: usize, _addrlen: usize) -> SyscallResult {
    if let Err(e) = validate_user_buffer(path_ptr as u64, 64) {
        return e;
    }

    let mut path_buf = [0u8; 64];
    let path_len = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0usize;
        while len < 63 && *ptr.add(len) != 0 {
            path_buf[len] = *ptr.add(len);
            len += 1;
        }
        len
    };
    if path_len == 0 { return errno::EINVAL; }

    let pid = crate::process::scheduler::current_pid().unwrap_or(0);

    let channel_id = match get_fd_channel(pid, fd as usize) {
        Some(id) => id,
        None => return ENOTSOCK,
    };

    let mut tbl = CHANNELS.lock();
    let ch = match tbl.get_mut(channel_id) {
        Some(c) => c,
        None => return errno::EBADF,
    };

    ch.bound_path = Some(path_buf);
    ch.server_state = Some(ServerState::Listening);
    0
}

// ——— sys_connect ——————————————————————————————————————————————————————————

/// sys_connect (#42) — connect a socket fd to a named server endpoint.
///
/// If no server is listening yet: returns -ENOENT.
/// If a server is listening: creates a channel pair and returns 0.
///
/// The server must subsequently call accept() to get the peer fd.
pub(super) fn sys_connect(fd: i32, path_ptr: usize, _addrlen: usize) -> SyscallResult {
    let pid_dbg = crate::process::scheduler::current_pid().unwrap_or(0);
    serial_println!("[DBG] sys_connect PID {} fd={}", pid_dbg, fd);
    if let Err(e) = validate_user_buffer(path_ptr as u64, 64) {
        return e;
    }

    let path_bytes = unsafe {
        let ptr = path_ptr as *const u8;
        let mut len = 0usize;
        while len < 63 && *ptr.add(len) != 0 { len += 1; }
        core::slice::from_raw_parts(ptr, len)
    };

    let pid = crate::process::scheduler::current_pid().unwrap_or(0);

    let client_channel_id = match get_fd_channel(pid, fd as usize) {
        Some(id) => id,
        None => return ENOTSOCK,
    };

    let mut tbl = CHANNELS.lock();

    // Find the server channel bound to this path
    let server_channel_id = match tbl.find_by_path(path_bytes) {
        Some(id) => id,
        None => return errno::ENOENT,
    };

    // Check it is actually listening
    let is_listening = tbl.get(server_channel_id)
        .map(|ch| ch.server_state == Some(ServerState::Listening))
        .unwrap_or(false);
    if !is_listening {
        return ECONNREFUSED;
    }

    // Allocate a server-side peer channel for this connection
    let server_peer_id = match tbl.alloc() {
        Some(id) => id,
        None => return errno::ENOMEM,
    };

    // Wire up the bidirectional pair:
    //   client_channel ↔ server_peer
    if let Some(ch) = tbl.get_mut(client_channel_id) {
        ch.peer = Some(server_peer_id);
    }
    if let Some(ch) = tbl.get_mut(server_peer_id) {
        ch.peer = Some(client_channel_id);
    }

    // Set server channel to PendingConnect; clear any stale accept waiter list
    if let Some(ch) = tbl.get_mut(server_channel_id) {
        ch.server_state = Some(ServerState::PendingConnect(server_peer_id));
    }

    drop(tbl);

    // If a process is blocked in sys_accept() for this server channel, wake it
    // and allocate the peer fd directly in its file table.
    let accept_waiter = {
        let mut aw = ACCEPT_WAITER.lock();
        if aw.as_ref().map(|w| w.server_channel_id == server_channel_id).unwrap_or(false) {
            aw.take()
        } else {
            None
        }
    };

    if let Some(waiter) = accept_waiter {
        serial_println!("[DBG] connect: waking accept waiter PID {}", waiter.pid);
        // Allocate the peer fd inside the blocked process.
        // The guard prevents the timer ISR from preempting while we hold SCHEDULER.
        let _irq = crate::process::irq_guard::InterruptGuard::new();

        let handle = alloc::boxed::Box::new(SocketHandle { channel_id: server_peer_id });
        let mut new_fd: i64 = errno::EINVAL;

        {
            let mut sched = crate::process::scheduler::local_scheduler();
            // Reset PendingConnect now that accept() is being satisfied
            CHANNELS.lock().get_mut(server_channel_id)
                .map(|ch| ch.server_state = Some(ServerState::Listening));

            for proc in sched.wait_queue.iter_mut() {
                if proc.pid.0 == waiter.pid
                    && matches!(proc.state, crate::process::ProcessState::Blocked)
                {
                    match proc.files.lock().allocate(handle) {
                        Ok(fd) => {
                            new_fd = fd as i64;
                            proc.trapframe.rax = fd as u64;
                        }
                        Err(_) => {
                            proc.trapframe.rax = (-22i64) as u64; // EINVAL
                        }
                    }
                    break;
                }
            }
            // Set the fd→channel mapping BEFORE wake() so the process
            // can never call recvmsg() with an unmapped fd.
            if new_fd >= 0 {
                set_fd_channel(waiter.pid, new_fd as usize, server_peer_id);
            }
            sched.wake(waiter.pid);
        }
    }

    0
}

// ——— sys_accept ——————————————————————————————————————————————————————————

/// sys_accept (#43) — accept the next incoming connection on a server socket.
///
/// Blocks until a client calls connect().
/// Returns a new fd for the server-side peer channel.
pub(super) fn sys_accept(fd: i32) -> SyscallResult {
    let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;
    let pid = crate::process::scheduler::current_pid().unwrap_or(0);

    let server_channel_id = match get_fd_channel(pid, fd as usize) {
        Some(id) => id,
        None => return ENOTSOCK,
    };

    // `irq` is deliberately never dropped on the slow (blocking) path below
    // — it ends in `jump_to_user` (`-> !`), so interrupts intentionally
    // stay off across that jump; see `sys_read`'s WouldBlock arm.
    let irq = crate::process::irq_guard::InterruptGuard::new();

    // Fast path: connection already pending from a previous connect().
    let pending = {
        let mut tbl = CHANNELS.lock();
        match tbl.get_mut(server_channel_id) {
            Some(ch) => {
                if let Some(ServerState::PendingConnect(peer_id)) = ch.server_state {
                    ch.server_state = Some(ServerState::Listening);
                    Some(peer_id)
                } else {
                    None
                }
            }
            None => return errno::EBADF,
        }
    };

    if let Some(peer_channel_id) = pending {
        let handle = alloc::boxed::Box::new(SocketHandle { channel_id: peer_channel_id });
        let new_fd = {
            let mut sched = crate::process::scheduler::local_scheduler();
            match sched.running_mut() {
                Some(proc) => match proc.files.lock().allocate(handle) {
                    Ok(fd) => fd as i64,
                    Err(_) => errno::EINVAL,
                },
                None => errno::ESRCH,
            }
        };
        if new_fd >= 0 { set_fd_channel(pid, new_fd as usize, peer_channel_id); }
        drop(irq);
        return new_fd;
    }

    // Slow path: register as waiter, block.
    // sys_connect() will allocate the fd for us and set trapframe.rax before
    // calling sched.wake().  We return from the syscall via iretq with rax
    // already set to the correct fd number — no code here runs after blocking.
    *ACCEPT_WAITER.lock() = Some(AcceptWaiter { pid, server_channel_id });

    let next_tf = {
        let mut sched = crate::process::scheduler::local_scheduler();
        sched.block_current(tf_ptr)
    };
    unsafe { crate::process::trapframe::jump_to_user(next_tf) }
}

// ——— sys_sendmsg ——————————————————————————————————————————————————————————

/// sys_sendmsg (#46) — send a message on a connected socket.
///
/// `msg_ptr` points to a user `IpcUserMsg { tag: u32, len: u32, data: [u8; 56] }`.
/// `tag`   — application-defined message type.
/// `len`   — how many bytes of `data` are valid (0..=56).
pub(super) fn sys_sendmsg(fd: i32, msg_ptr: u64, _flags: u32) -> SyscallResult {
    if let Err(e) = validate_user_buffer(msg_ptr, 64) {
        return e;
    }

    // Read the IpcUserMsg from user memory (user page table active)
    let (tag, len, data) = unsafe {
        let ptr = msg_ptr as *const u8;
        let tag  = u32::from_le_bytes([*ptr, *ptr.add(1), *ptr.add(2), *ptr.add(3)]);
        let len  = u32::from_le_bytes([*ptr.add(4), *ptr.add(5), *ptr.add(6), *ptr.add(7)]);
        let len  = core::cmp::min(len, 56) as usize;
        let mut data = [0u8; 56];
        core::ptr::copy_nonoverlapping(ptr.add(8), data.as_mut_ptr(), len);
        (tag, len as u32, data)
    };

    let msg = IpcMessage { tag, len, data };

    let pid = crate::process::scheduler::current_pid().unwrap_or(0);

    let channel_id = match get_fd_channel(pid, fd as usize) {
        Some(id) => id,
        None => return ENOTSOCK,
    };

    // Check if a process is blocked in recvmsg() on the peer channel.
    // If so, deliver the message directly to its user buffer (zero-copy from
    // the kernel's point of view) and wake it — no need to enqueue.
    let peer_id = {
        let tbl = CHANNELS.lock();
        match tbl.get(channel_id).and_then(|ch| ch.peer) {
            Some(id) => id,
            None => return ENOTCONN,
        }
    };

    let recv_waiter = {
        let mut rw = RECV_WAITER.lock();
        if rw.as_ref().map(|w| w.channel_id == peer_id).unwrap_or(false) {
            rw.take()
        } else {
            None
        }
    };

    if let Some(waiter) = recv_waiter {
        // Fast delivery: write directly to the pre-translated physical address
        // stored in the waiter — no page-table walk needed.
        let phys_offset = crate::memory::physical_memory_offset().as_u64();
        if waiter.phys_buf != 0 {
            let dst = (phys_offset + waiter.phys_buf) as *mut u8;
            unsafe {
                core::ptr::write_bytes(dst, 0, 64);
                core::ptr::copy_nonoverlapping(msg.tag.to_le_bytes().as_ptr(), dst,       4);
                core::ptr::copy_nonoverlapping(msg.len.to_le_bytes().as_ptr(), dst.add(4), 4);
                core::ptr::copy_nonoverlapping(msg.data.as_ptr(),              dst.add(8), msg.len as usize);
            }
        }

        // Wake the receiver, setting its syscall return value — single scan
        // of the wait_queue (previously: separate write-rax loop + wake scan).
        {
            let mut sched = crate::process::irq_guard::SchedGuard::lock();
            sched.wake_with_retval(waiter.pid, 64);
        }
        return 64;
    }

    // No waiter — enqueue for future recvmsg().
    let enqueued = {
        let mut tbl = CHANNELS.lock();
        match tbl.get_mut(peer_id) {
            Some(ch) => ch.enqueue(msg),
            None => return EPIPE,
        }
    };
    if !enqueued { return EAGAIN; }

    // Wake any poll/epoll waiter watching peer_id for POLLIN.
    // Called after CHANNELS lock is released.
    super::poll::poll_wakeup_for_channel(peer_id);

    64
}

/// Write a Message into a user buffer using physical address translation.
///
/// Used by sys_sendmsg to deliver a message to a blocked sys_recvmsg without
/// going through the queue (same technique as stdin_wakeup).
fn write_msg_to_user(
    addr_space: &crate::memory::address_space::AddressSpace,
    user_buf: u64,
    msg: &IpcMessage,
    phys_offset: x86_64::VirtAddr,
) {
    use x86_64::{VirtAddr, structures::paging::{Page, Size4KiB}};

    // The Message is 64 bytes; it might straddle a page boundary (unlikely for
    // aligned allocations, but we handle it field-by-field for safety).
    // For simplicity, assume the 64-byte Message is within a single 4K page
    // (the compiler aligns Message to 64 bytes, so it never crosses a page).
    let page = Page::<Size4KiB>::containing_address(VirtAddr::new(user_buf));
    let offset = user_buf & 0xFFF;

    if let Some(frame) = unsafe { addr_space.translate_page(page) } {
        let dst_va = phys_offset + frame.start_address().as_u64() + offset;
        let dst = dst_va.as_mut_ptr::<u8>();
        unsafe {
            // Zero the 64-byte slot
            core::ptr::write_bytes(dst, 0, 64);
            // tag (4 bytes)
            core::ptr::copy_nonoverlapping(msg.tag.to_le_bytes().as_ptr(), dst, 4);
            // len (4 bytes)
            core::ptr::copy_nonoverlapping(msg.len.to_le_bytes().as_ptr(), dst.add(4), 4);
            // data
            core::ptr::copy_nonoverlapping(msg.data.as_ptr(), dst.add(8), msg.len as usize);
        }
    }
}

// (sys_recvmsg follows)

// ——— sys_recvmsg ——————————————————————————————————————————————————————————

/// sys_recvmsg (#47) — receive a message from a connected socket.
///
/// `msg_ptr` points to a user buffer (64 bytes) that will receive the message.
/// Blocks if no message is available.
pub(super) fn sys_recvmsg(fd: i32, msg_ptr: u64, _flags: u32) -> SyscallResult {
    if let Err(e) = validate_user_buffer(msg_ptr, 64) {
        return e;
    }

    let tf_ptr = CURRENT_SYSCALL_TF.load(Ordering::Relaxed) as *const TrapFrame;
    let pid = crate::process::scheduler::current_pid().unwrap_or(0);

    let channel_id = match get_fd_channel(pid, fd as usize) {
        Some(id) => id,
        None => return ENOTSOCK,
    };

    // `irq` is deliberately never dropped on the slow (blocking) path below
    // — it ends in `jump_to_user` (`-> !`), so interrupts intentionally
    // stay off across that jump; see `sys_read`'s WouldBlock arm.
    let irq = crate::process::irq_guard::InterruptGuard::new();

    // Fast path: message already queued.
    let queued = CHANNELS.lock().get_mut(channel_id).and_then(|ch| ch.dequeue());

    if let Some(m) = queued {
        drop(irq);
        unsafe {
            let ptr = msg_ptr as *mut u8;
            ptr.write_bytes(0, 64);
            core::ptr::copy_nonoverlapping(m.tag.to_le_bytes().as_ptr(), ptr,       4);
            core::ptr::copy_nonoverlapping(m.len.to_le_bytes().as_ptr(), ptr.add(4), 4);
            core::ptr::copy_nonoverlapping(m.data.as_ptr(),              ptr.add(8), m.len as usize);
        }
        return 64;
    }

    // Slow path: block.
    // Pre-translate the user buffer VA → physical address so that the sender
    // (sys_sendmsg) can write directly to physical memory without a page-table
    // walk on the delivery fast path.
    let phys_buf = {
        use x86_64::{VirtAddr, structures::paging::{Page, Size4KiB}};
        let page   = Page::<Size4KiB>::containing_address(VirtAddr::new(msg_ptr));
        let offset = msg_ptr & 0xFFF;
        // cli is already set; safe to acquire scheduler read-only.
        let sched = crate::process::scheduler::local_scheduler();
        sched.running_ref()
            .and_then(|proc| unsafe { proc.address_space.translate_page(page) })
            .map(|frame| frame.start_address().as_u64() + offset)
            .unwrap_or(0)
        // sched guard dropped here
    };

    *RECV_WAITER.lock() = Some(RecvWaiter { pid, channel_id, phys_buf });

    let next_tf = {
        let mut sched = crate::process::scheduler::local_scheduler();
        sched.block_current(tf_ptr)
    };
    unsafe { crate::process::trapframe::jump_to_user(next_tf) }
}

use errno::*;

