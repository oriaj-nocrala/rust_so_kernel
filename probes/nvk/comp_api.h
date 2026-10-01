/*
 * comp_api.h: what the GPU compositor's renderer is given to draw (G5 layer 4). The window manager (the `gui` crate, in Rust) turns its draw
 * list into these; the renderer (comp_render.h, C on NVK) does not know the protocol, the clients or the window manager. Plain data, mirrored
 * by vk-comp/src/ffi.rs.
 */
#ifndef COMP_API_H
#define COMP_API_H

#include <stddef.h>
#include <stdint.h>

#define CR_FILL 0   /* rect (x, y, w, h) in `color` (0x00RRGGBB) */
#define CR_GPU 1    /* rect shows the GPU buffer `key` (cr_import) from pixel (sx, sy) */
#define CR_CPU 2    /* rect shows `px` (`src_w` pixels per row, `npx` in all) from (sx, sy); uploaded when `version` differs from the last upload under `key` */

struct cr_op {
   uint32_t kind;
   uint32_t color;
   int32_t x, y, w, h;
   int32_t sx, sy;
   uint64_t key;            /* GPU: the buffer's handle; CPU: a number of the caller's that names the source (a window, a title, the cursor) */
   uint64_t version;        /* CPU */
   const uint32_t *px;      /* CPU: valid during the call */
   uint64_t npx;
   int32_t src_w;
   uint32_t keyed;          /* CPU: a pixel whose top byte is 0 is transparent (the cursor) */
};


struct cr_stats {
   uint32_t frames, draws, draws_max, imports, drops, uploads;
   /* the last cr_frame's phases, microseconds: acquire the image, record and submit the draw, present (the WSI's copy to the scanout buffer,
    * its CPU wait for that copy and the PRESENT ioctl) */
   uint32_t acquire_us, render_us, present_us;
   uint32_t upload_kb;      /* KiB of CPU windows' pixels the frames copied (only the rows that changed) */
};

/* The renderer's entry points (comp_vk.c), what the compositor program in Rust calls. All on one thread. */
int cr_init(int headless, uint32_t *width, uint32_t *height);   /* Vulkan, the screen (the WSI's direct path) and the pipeline; 0, or a negative step (it printed why) */
int cr_import(uint64_t handle, int fd, uint64_t size, uint32_t stride_bytes);   /* a client's GPU buffer; takes the descriptor */
void cr_drop(uint64_t handle);                                  /* nothing refers to it any more: freed at the next frame start */
uint64_t cr_wait(void);                                         /* waits for the frame in flight; its number, 0 if none: that frame is done */
int cr_frame(const struct cr_op *ops, size_t n, uint64_t epoch); /* acquire an image, draw `ops`, present: 0, or negative */
int cr_wait_flip(void);                                         /* sleeps until the frame just presented is on the screen (its flip landed); 0, or negative (timeout, no display) */
void cr_get_stats(struct cr_stats *out);
void cr_shutdown(void);

#endif
