# IPC: pipes, AF_UNIX sockets, pseudo-terminals

## Shared pattern (sockets and ptys)

- **All the rules live in a host-tested crate** (`usock`, `tty`). **Nothing there blocks**: an operation that can't complete returns `Again`, and wakeups/signals come back as data (`Wakes`, `tty::pty::Effects`).
- The kernel adapter (`kernel/src/ipc/unix.rs`, `ipc/pty.rs`) owns the globals, puts the object behind a `FileHandle`, and blocks.
- **Blocking restarts the syscall**: register as a waiter, rewind the saved `rip` by 2, park. The call re-executes when the process runs again; a signal handler runs first (`SA_RESTART` semantics).
  - `register_retry` (used from `FileHandle::read`/`write`) **requires the caller to actually block afterwards**. Returning normally after rewinding re-enters `syscall` with a return value in `rax`.
  - An ioctl can't block: it restarts by rewinding and returning the syscall number.
- Globals (`SOCKETS`, `PTYS`, `WAITERS`) are `diag::IrqMutex`es. **Never take `SCHEDULER` while holding one**: compute the effects, release, then apply them.
- The wake path saves and restores IF (`without_interrupts`), never unconditionally re-enables it: `UnixSocketHandle::drop` runs inside `sys_exit`, which must keep IF=0.

## Pipes (`kernel/src/process/pipe.rs`)

- Unlike sockets, the waker completes the sleeper's read, copying into the sleeper's memory through `AddressSpace::copy_to_user`.
- One FIFO of waiters per end.
- Tests: `pipe_cow_test`, `pipe_multi_test`.
- Not implemented: `poll` on a pipe is always "ready".

## AF_UNIX sockets (`usock/`, `ipc/unix.rs`, `process/syscall/ipc.rs`)

- `usock::SocketTable<F>`: stream and datagram sockets, the bind registry (paths and the abstract namespace), backlog, half-close, `SO_*`, `SCM_RIGHTS`, `sockaddr_un` parsing. The kernel uses `F = Box<dyn FileHandle>`; host tests use integers.
- `syscall/ipc.rs` is the user-memory boundary: sockaddrs, iovecs, cmsgs, and the EAGAIN-vs-park decision.
- **fd → socket**: `FileHandle::socket_id()` (a `dyn FileHandle` can't be downcast in `no_std`). `poll`/`epoll` snapshot that mapping (`SocketMap`), because a waker can't reach another process's fd table.
- **`bind()` to a path has two halves**: an `S_IFSOCK` node in the filesystem (ramfs only, `Inode::mksocket`) *plus* a registry entry, which is what `connect()` resolves. This gives Linux's errors:
  - no node → `ENOENT`;
  - a node with no live socket → `ECONNREFUSED`;
  - binding again while the node exists → `EADDRINUSE`.

  Abstract names (`sun_path[0]==0`) skip the filesystem.
- A stream `connect()` completes or fails immediately: it creates the server-side socket itself, as Linux does.
- `SOCK_SEQPACKET` (`SockType::SeqPacket`): connects like a stream (`listen`/`accept`/`connect`, `socketpair`, half-close, EOF when the peer is gone) and keeps message boundaries like a datagram socket (one send = one recv, a short buffer truncates and drops the rest, `MSG_TRUNC` reports the real length, a message bigger than the receive buffer is `EMSGSIZE`). Rust's `Command` needs it for its exec-error pipe whenever it forks (uid/gid/`pre_exec`).
- Out of scope: `SO_PEERCRED` (ids are bookkeeping only), `MSG_OOB`, `EINPROGRESS`, `AF_INET`.
- Tests: `cd usock && cargo test`, `hw_tests::unix_socket_handle_roundtrip`, `socket_test` (end-to-end through mlibc), `ipc_ping`, `poll_test`.

## Pseudo-terminals (`tty/`, `ipc/pty.rs`)

- `/dev/ptmx` creates a pair (max 16). `/dev/pts/<n>` is the slave (listed while the master is open). `/dev/tty` is the caller's controlling terminal (`ENXIO` without one).
- The `tty` crate has termios (this port's layout), the line discipline (canonical mode, echo, `ISIG`, `VMIN`, `OPOST`), job control, and hangup/`EIO`/`POLLHUP`/`SIGWINCH`.
- **Controlling terminal**: `Process::sid`/`ctty` are inherited by fork and cleared by `setsid`. A session leader acquires one by opening a slave without `O_NOCTTY`, or with `TIOCSCTTY`.
- **Job control**: from a background group, a read (`SIGTTIN`), a write with `TOSTOP`, or `TCSETS*`/`TIOCSPGRP` (`SIGTTOU`) signals the group and restarts the call. If that signal is ignored/blocked, or the group is orphaned: `EIO` (a write goes through).
- **The console is not a pty.** Its termios and foreground group are globals in `kernel/src/tty.rs`, and its input is the keyboard ring.
- Not implemented: `VTIME` timing, `IXON`/`IXOFF`, hangup when the session leader dies (closing the master is the hangup).
- `poll`/`epoll` are real on both ends.
- mlibc has `posix_openpt`/`grantpt`/`unlockpt`/`ptsname`/`ttyname`/`tcflush`.
- BusyBox `script` needs `SHELL=/tmp/bin/sh` (there is no `/bin/sh`).
- Tests: `cd tty && cargo test`, `pty_test`.
