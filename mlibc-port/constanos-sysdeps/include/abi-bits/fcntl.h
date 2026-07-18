#ifndef _ABIBITS_FCNTL_H
#define _ABIBITS_FCNTL_H

// Values must match kernel/src/fs/types.rs::OpenFlags exactly — this
// kernel reads these bits directly off the raw `flags` argument (access
// mode in bits 0-1, CREAT/TRUNC/APPEND/DIRECTORY each their own bit), no
// translation layer either way. The previous values here were inherited
// from a dripos-style template with a completely different numbering
// scheme (e.g. O_CREAT=0x10, O_RDONLY=2) that happened to compile but
// silently broke O_CREAT (bit never matched the kernel's 0o100) — see
// [[mlibc-port-and-kernel-bugs]] memory for the class of bug this is.
/* reserve 3 bits for the access mode */
#define O_ACCMODE 0003
#define O_RDONLY 00
#define O_WRONLY 01
#define O_RDWR 02

/* these flags get their own bit, standard Linux x86-64 positions */
#define O_CREAT     0000100
#define O_EXCL      0000200
#define O_NOCTTY    0000400
#define O_TRUNC     0001000
#define O_APPEND    0002000
#define O_NONBLOCK  0004000
#define O_DSYNC     0010000
#define O_ASYNC     0020000
#define O_DIRECT    0040000
#define O_LARGEFILE 0100000
#define O_DIRECTORY 0200000
#define O_NOFOLLOW  0400000
#define O_NOATIME   01000000
#define O_CLOEXEC   02000000
#define O_SYNC      (04000000 | O_DSYNC)
#define O_RSYNC     O_SYNC
#define O_PATH      010000000
#define O_TMPFILE   (020000000 | O_DIRECTORY)

// Same story as the O_* block above: real Linux x86-64 values, not the
// dripos-template ones — kernel/src/process/syscall.rs::sys_fcntl matches
// on these exact numbers (F_DUPFD=0 vs. this header's old F_DUPFD=1 would
// have silently landed on F_GETFD's stub instead of actually duplicating).
/* constants for fcntl()'s command argument */
#define F_DUPFD 0
#define F_GETFD 1
#define F_SETFD 2
#define F_GETFL 3
#define F_SETFL 4
#define F_GETLK 5
#define F_SETLK 6
#define F_SETLK64 F_SETLK
#define F_SETLKW 7
#define F_SETLKW64 F_SETLKW
#define F_SETOWN 8
#define F_GETOWN 9
#define F_DUPFD_CLOEXEC 1030

/* constants for struct flock's l_type member */
#define F_RDLCK 1
#define F_UNLCK 2
#define F_WRLCK 3

/* constants for fcntl()'s additional argument of F_GETFD and F_SETFD */
#define FD_CLOEXEC 1

/* Used by mmap */
#define F_SEAL_SHRINK 0x0002
#define F_SEAL_GROW   0x0004
#define F_SEAL_WRITE  0x0008
#define F_SEAL_SEAL   0x0010
#define F_SETPIPE_SZ  1031
#define F_GETPIPE_SZ  1032
#define F_ADD_SEALS   1033
#define F_GET_SEALS   1034

#define AT_EMPTY_PATH 1
#define AT_SYMLINK_FOLLOW 2
#define AT_SYMLINK_NOFOLLOW 4
#define AT_REMOVEDIR 8
#define AT_EACCESS 512
#define AT_NO_AUTOMOUNT 1024
#define AT_STATX_SYNC_AS_STAT 0
#define AT_STATX_FORCE_SYNC 2048
#define AT_STATX_DONT_SYNC 4096
#define AT_STATX_SYNC_TYPE 6144

#define AT_FDCWD -100

#define POSIX_FADV_NORMAL 1
#define POSIX_FADV_SEQUENTIAL 2
#define POSIX_FADV_NOREUSE 3
#define POSIX_FADV_DONTNEED 4
#define POSIX_FADV_WILLNEED 5
#define POSIX_FADV_RANDOM 6

#endif /* _ABITBITS_FCNTL_H */
