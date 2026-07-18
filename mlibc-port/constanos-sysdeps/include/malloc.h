#ifndef _MALLOC_H
#define _MALLOC_H

// Copied from mlibc/options/linux/include/malloc.h (that option is
// disabled for this port). Deliberately does NOT define M_TRIM_THRESHOLD/
// M_MMAP_THRESHOLD/M_TOP_PAD/mallopt() — BusyBox's only use of them
// (libbb/appletlib.c) is entirely `#ifdef`-guarded, so leaving them
// undefined just compiles those tuning calls out instead of requiring a
// real mallopt() implementation this kernel's slab allocator has no
// equivalent for.

#ifdef __cplusplus
extern "C" {
#endif

#include <bits/size_t.h>
#include <mlibc-config.h>

#ifndef __MLIBC_ABI_ONLY

/* [7.22.3] Memory management functions */
void *calloc(size_t __count, size_t __size);
void free(void *__pointer);
void *malloc(size_t __size);
void *realloc(void *__pointer, size_t __size);
void *memalign(size_t __alignment, size_t __size);

#if __MLIBC_GLIBC_OPTION
#include <bits/glibc/glibc_malloc.h>
#endif

#endif /* !__MLIBC_ABI_ONLY */

#ifdef __cplusplus
}
#endif

#endif /* _MALLOC_H */
