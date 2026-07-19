#ifndef _ABIBITS_SEEK_WHENCE_H
#define _ABIBITS_SEEK_WHENCE_H

/* Linux values (mlibc/abis/linux/seek-whence.h), NOT the mlibc-generic
 * abi this file was originally copied from (SEEK_SET 3 there!) — the
 * kernel's compute_seek() speaks Linux whence numbers (0/1/2), and the
 * old SEEK_SET=3 made every lseek(fd, n, SEEK_SET) return EINVAL while
 * SEEK_CUR/SEEK_END coincidentally matched. Found via DOOM's WAD loader:
 * M_FileLength's SEEK_END probe worked, the fseek back to 0 didn't, so
 * every subsequent read returned EOF ("empty file" symptoms — earlier
 * misattributed to ATA corruption on the ext2 route). */
#define SEEK_SET 0
#define SEEK_CUR 1
#define SEEK_END 2
#define SEEK_DATA 3
#define SEEK_HOLE 4

#endif /* _ABIBITS_SEEK_WHENCE_H */
