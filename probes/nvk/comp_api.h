/*
 * comp_api.h: what the GPU compositor's renderer is given to draw (G5 layer 4). The window manager (the `gui` crate, in Rust) turns its draw
 * list into these; the renderer (comp_render.h, C on NVK) does not know the protocol, the clients or the window manager. Plain data, mirrored
 * by vk-comp/src/ffi.rs.
 */
#ifndef COMP_API_H
#define COMP_API_H

#include <stddef.h>
#include <stdint.h>

#define CR_FILL 0   /* rect (x, y, w, h) in `color` (0x00RRGGBB), opaque */
#define CR_GPU 1    /* rect shows the GPU buffer `key` (cr_import) from pixel (sx, sy) */
#define CR_CPU 2    /* rect shows `px` (`src_w` pixels per row, `npx` in all) from (sx, sy); uploaded when `version` differs from the last upload under `key` */
#define CR_SHAPE 3  /* the box (x, y, w, h) drawn as `shape` says: rounded, a gradient, a border, a shadow (which may reach past the box) */

/* How a GPU or CPU source's pixels cover what is under them (`alpha`). */
#define CR_OPAQUE 0   /* 0x??RRGGBB, the top byte ignored */
#define CR_KEYED 1    /* a pixel whose top byte is 0 is transparent, any other is opaque (the cursor) */
#define CR_PREMUL 2   /* premultiplied 0xAARRGGBB (what crate `img` decodes), composited "over" */

/* A CR_SHAPE. Colours are 0xAARRGGBB with straight (not premultiplied) alpha: 0xFF opaque, 0 invisible. All of it is computed per pixel
 * (comp.frag), so the edges are anti-aliased and nothing is rasterised ahead. */
struct cr_shape {
   float radius;            /* corner radius in pixels (clamped to half the box's shorter side) */
   float border;            /* border width in pixels, inside the box; 0 = none */
   float split;             /* the gradient: c0 -> c1 over [0, split], c2 -> c3 over [split, 1] of the box's height (or width); c1 == c2 is
                             * one 3-stop gradient, split = 1 a plain c0 -> c1, a hard step at `split` is the glossy look */
   float shadow_blur;       /* the shadow's soft edge, pixels on each side of the outline; 0 = a hard shadow */
   uint32_t c[4];
   uint32_t border_color;
   uint32_t shadow_color;   /* alpha 0 = no shadow */
   int32_t shadow_dx, shadow_dy;
   uint32_t horizontal;     /* 1: the gradient runs left to right instead of top to bottom */
};

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
   uint32_t alpha;          /* GPU, CPU: CR_OPAQUE, CR_KEYED or CR_PREMUL */
   struct cr_shape shape;   /* SHAPE */
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
