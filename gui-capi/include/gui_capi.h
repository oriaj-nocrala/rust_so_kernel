// gui_capi.h — the compositor's window manager (the `gui` crate: protocol, clients, surfaces, stacking, focus, the pointer, window
// management) behind a C ABI. Nothing here blocks or touches a socket: the program feeds in what its clients sent and its input devices
// produced, and carries out what comes back (events to send, descriptors to close, GPU buffers to import, what to draw).
//
//   gui_comp *c = gui_new(1920, 1080);  gui_enable_gpu_buffers(c);
//   per client socket:  id = gui_add_client(c);  ... gui_client_data(c, id, bytes, n, fds, nfds) on every recvmsg ...
//   per frame:          epoch = gui_draw_list(c);  for i < gui_draw_count(c): gui_draw_get(c, i, &op); draw it;
//                       when the GPU is done with that frame: gui_gpu_frame_done(c, epoch);
//   after each call:    drain gui_pop_event / gui_pop_disconnect / gui_pop_fd_to_close / gui_pop_gpu_op
//
// The semantics of GPU buffers (import, release, drop) are in docs/reference/graphics.md "GPU buffers" and gui/src/compositor.rs.
#ifndef GUI_CAPI_H
#define GUI_CAPI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct gui_comp gui_comp;

gui_comp *gui_new(int32_t width, int32_t height);
void gui_free(gui_comp *c);
/* Accept create_gpu_buffer (off by default: a client that sends one is disconnected). */
void gui_enable_gpu_buffers(gui_comp *c);

/* ---- clients ---- */
uint32_t gui_add_client(gui_comp *c);
void gui_remove_client(gui_comp *c, uint32_t client);
/* Bytes (and the descriptors that came with them, in order) of one recvmsg of `client`. Pool descriptors are mapped here; every descriptor the
 * compositor is done with comes back through gui_pop_fd_to_close. */
void gui_client_data(gui_comp *c, uint32_t client, const uint8_t *bytes, size_t len, const int32_t *fds, size_t nfds);

/* ---- what to carry out (each returns 0 when there is nothing) ---- */
/* The next event to send, already encoded (wire format), to `*client`; the length, 0 when none. `cap` should be at least 4096. */
size_t gui_pop_event(gui_comp *c, uint32_t *client, uint8_t *buf, size_t cap);
/* The next client to disconnect (after its error event was sent). */
int gui_pop_disconnect(gui_comp *c, uint32_t *client);
/* The next descriptor to close. */
int gui_pop_fd_to_close(gui_comp *c, int32_t *fd);

#define GUI_GPU_IMPORT 1
#define GUI_GPU_DROP 2
struct gui_gpu_op {
    uint32_t kind;   /* GUI_GPU_IMPORT / GUI_GPU_DROP */
    uint32_t _pad;
    uint64_t handle; /* the compositor's name for the buffer, used by draw ops */
    int32_t fd;      /* IMPORT: the descriptor; the host owns it (close it) */
    int32_t width, height;
    uint32_t stride; /* bytes per row */
    uint64_t size;   /* bytes of the descriptor */
};
/* The next GPU-buffer operation, in order. DROP: free the buffer once every frame started before it is done. */
int gui_pop_gpu_op(gui_comp *c, struct gui_gpu_op *op);

/* ---- input (evdev codes: KEY_*, BTN_*) ---- */
void gui_set_time(gui_comp *c, uint32_t ms);
void gui_pointer_motion(gui_comp *c, int32_t dx, int32_t dy);
void gui_pointer_button(gui_comp *c, uint32_t code, int pressed);
void gui_key(gui_comp *c, uint32_t code, int pressed);
int gui_quit_requested(const gui_comp *c);
/* The frame callbacks (`frame` requests) fire: call once per displayed frame, with its time. */
void gui_frame_done(gui_comp *c, uint32_t ms);
int gui_has_frame_callbacks(const gui_comp *c);

/* ---- drawing ---- */
int gui_has_damage(const gui_comp *c);

#define GUI_DRAW_FILL 0    /* rect, color */
#define GUI_DRAW_GPU 1     /* rect shows buffer `handle` from pixel (sx, sy) */
#define GUI_DRAW_CPU 2     /* rect shows gui_cpu_content(client, surface) (src_w x src_h) from (sx, sy); `version` changes with the pixels */
#define GUI_DRAW_TITLE 3   /* a title to paint over its bar: id, focused, area (x, y, w, h), clip (clip_*); the text is gui_title(c, i, ...) */
#define GUI_DRAW_CURSOR 4  /* the pointer, hotspot at (x, y); the bitmap is gui_cursor_bitmap */
#define GUI_DRAW_SHAPE 5   /* the box (x, y, w, h) drawn as the shape_* fields say (comp_api.h's struct cr_shape; gui::theme::Shape), only inside
                            * clip_* when clip_w > 0 */
struct gui_draw_op {
    uint32_t kind;
    uint32_t color;      /* FILL: 0x00RRGGBB */
    int32_t x, y, w, h;  /* destination rect on screen (CURSOR: x, y only; TITLE: the area) */
    int32_t sx, sy;      /* GPU, CPU: offset into the source */
    uint64_t handle;     /* GPU */
    uint32_t client, surface; /* CPU */
    uint64_t version;    /* CPU */
    int32_t src_w, src_h;/* CPU: size of the surface's pixels */
    uint32_t id;         /* TITLE: toplevel id, stable while the window is mapped */
    uint32_t focused;    /* TITLE */
    int32_t clip_x, clip_y, clip_w, clip_h; /* TITLE; SHAPE (clip_w 0: none) */
    uint32_t title_fg;   /* TITLE: 0x00RRGGBB */
    uint32_t title_shadow; /* TITLE: 0xAARRGGBB, alpha 0 = none */
    float shape_radius, shape_border, shape_split, shape_shadow_blur;   /* SHAPE */
    uint32_t shape_c[4], shape_border_color, shape_shadow_color;
    int32_t shape_shadow_dx, shape_shadow_dy;
    uint32_t shape_horizontal;
    uint32_t premul;     /* CPU: the pixels are premultiplied ARGB, drawn "over" (else opaque) */
    float shape_backdrop_blur; /* SHAPE: glass, the fill over what is behind blurred by this radius (0: none) */
};
/* The draw list's look (gui::theme): "luna" (the default), "9x". 0, or -1 for an unknown name. F12 cycles them too.
 * The panel is told (a `theme` event); in a look with a taskbar the strip is a SHAPE under the panel's surface. */
int gui_set_theme(gui_comp *c, const char *name);
/* Builds the draw list for the next frame (the whole screen, back to front, clipped), takes the damage, and returns the frame's number. */
uint64_t gui_draw_list(gui_comp *c);
size_t gui_draw_count(const gui_comp *c);
/* Operation `i` of the list built last. 0 on success, -1 past the end. */
int gui_draw_get(const gui_comp *c, size_t i, struct gui_draw_op *op);
/* The title text of TITLE operation `i` (NUL-terminated, valid until the next gui_draw_list), or NULL. */
const char *gui_title(const gui_comp *c, size_t i);
/* The pixels of a pool window (XRGB8888, rows src_w pixels long), or NULL. Valid until the next call that changes the compositor. */
const uint32_t *gui_cpu_content(const gui_comp *c, uint32_t client, uint32_t surface, size_t *len_pixels);
/* The frame whose GPU work is finished (frames complete in order): GPU buffers replaced before it was started are released to their clients. */
void gui_gpu_frame_done(gui_comp *c, uint64_t epoch);

/* The pointer's bitmap: GUI_CURSOR_H rows of GUI_CURSOR_W characters, 'X' black, '.' white, ' ' transparent. */
#define GUI_CURSOR_W 11
#define GUI_CURSOR_H 16
const char *gui_cursor_bitmap(size_t row);

#ifdef __cplusplus
}
#endif
#endif
