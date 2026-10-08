// constanos_gui_vk.h — a window for a Vulkan program: the connection to the compositor and the hooks of constanos_vk_window.h, on top of
// constanos_gui_wire.h. Header-only. Include <vulkan/vulkan.h> first (the program links NVK statically; see constanos_vk_window.h).
//
//   struct gvk_window w;
//   gvk_open(&w, "snake3d");                       // connects ($GUI_DISPLAY, else /tmp/gui-0), makes the surface, waits for `configure`
//   gvk_surface_create(&w, instance, &surface);    // the VkSurfaceKHR; then VK_KHR_swapchain as anywhere (FIFO, B8G8R8A8, any size)
//   ... gvk_next_event(&w, &ev) for keys and the pointer; ev.type == GVK_CLOSE when the user closes the window ...
//
// The program is the connection's only reader: the swapchain's acquire calls back into gvk_pump() while it waits for a release, so events
// keep being handled (and queued for gvk_next_event) meanwhile.

#ifndef CONSTANOS_GUI_VK_H
#define CONSTANOS_GUI_VK_H

#include <errno.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/un.h>
#include <unistd.h>

#include "constanos_gui_wire.h"
#include "constanos_vk_window.h"

#define GVK_KEY 1       // code = a Linux KEY_*/BTN_*, value 1 press / 0 release
#define GVK_MOTION 2    // code = 0, value unused; x/y in `x`, `y` (surface-local)
#define GVK_REL 3       // pointer locked: code 0 = dx, 1 = dy (screen sign: positive down), value = the motion
#define GVK_FOCUS 4     // value 1 gained, 0 lost
#define GVK_CLOSE 5     // the user asked for the window to close
#define GVK_CONFIGURE 6 // value = w, code unused, y = h (the compositor's suggestion; the program picks its own size)
#define GVK_WHEEL 8     // value = wheel notches, positive away from the user (scroll up); x/y unused
#define GVK_RESIZE 7    // value = w, y = h: the size the compositor gives the window (maximize, a resize drag, F11 fullscreen); a program that sent
                        // gvk_set_resizable makes its swapchain this size, and the next buffer it sends is the window's new size

struct gvk_event {
    uint16_t type;
    uint16_t code;
    int32_t value;
    int32_t x, y;
};

#define GVK_QUEUE 256

struct gvk_window {
    int sock;
    uint32_t surface_id;
    uint32_t next_id;
    uint8_t rx[8192];
    size_t rxlen;
    int closed;           // the connection is gone
    int cfg_w, cfg_h;     // the last `configure`
    struct gvk_event q[GVK_QUEUE];
    unsigned qhead, qtail;
    // what the swapchain asked of the hooks, for tests
    unsigned buffers_sent, buffers_destroyed, commits, releases, waits, throttled, throttle_timeouts;
    uint32_t frame_cb;    // the `frame` callback of the last commit, 0 once the compositor said it showed it
    uint64_t surface_handle;  // the VkSurfaceKHR, to tell the swapchain what the compositor released
    struct constanos_window hooks;
};

static void gvk_push(struct gvk_window *w, struct gvk_event e) {
    if (w->qtail - w->qhead >= GVK_QUEUE) return; // full: drop
    w->q[w->qtail++ % GVK_QUEUE] = e;
}

static int gvk_next_event(struct gvk_window *w, struct gvk_event *e) {
    if (w->qhead == w->qtail) return 0;
    *e = w->q[w->qhead++ % GVK_QUEUE];
    return 1;
}

static int gvk_send(struct gvk_window *w, struct guiw_out *o) {
    if (o->overflow || w->closed) return -EIO;
    struct msghdr mh;
    char ctl[CMSG_SPACE(sizeof(int) * 4)];
    struct iovec iov = { o->bytes, o->len };
    memset(&mh, 0, sizeof(mh));
    mh.msg_iov = &iov;
    mh.msg_iovlen = 1;
    if (o->nfds > 0) {
        memset(ctl, 0, sizeof(ctl));
        mh.msg_control = ctl;
        mh.msg_controllen = CMSG_SPACE(sizeof(int) * o->nfds);
        struct cmsghdr *c = CMSG_FIRSTHDR(&mh);
        c->cmsg_level = SOL_SOCKET;
        c->cmsg_type = SCM_RIGHTS;
        c->cmsg_len = CMSG_LEN(sizeof(int) * o->nfds);
        memcpy(CMSG_DATA(c), o->fds, sizeof(int) * o->nfds);
    }
    long n = sendmsg(w->sock, &mh, 0);
    memset(o, 0, sizeof(*o));
    if (n < 0) { w->closed = 1; return -errno; }
    return 0;
}

static void gvk_dispatch(struct gvk_window *w) {
    size_t off = 0;
    for (;;) {
        struct guiw_msg m;
        int n = guiw_next(w->rx + off, w->rxlen - off, &m);
        if (n < 0) { w->closed = 1; break; }
        if (n == 0) break;
        off += (size_t)n;
        int na = guiw_nargs(&m);
        if (m.object == GUIW_COMPOSITOR) {
            if (m.opcode == GUIW_EV_ERROR && na >= 2) {
                fprintf(stderr, "gvk: protocol error %u on object %u\n", guiw_arg(&m, 1), guiw_arg(&m, 0));
                w->closed = 1;
            }
        } else if (m.object == w->surface_id) {
            uint32_t a = na > 0 ? guiw_arg(&m, 0) : 0, b = na > 1 ? guiw_arg(&m, 1) : 0;
            struct gvk_event e;
            memset(&e, 0, sizeof(e));
            switch (m.opcode) {
            case GUIW_EV_CONFIGURE:
                w->cfg_w = (int)a; w->cfg_h = (int)b;
                e.type = GVK_CONFIGURE; e.value = (int)a; e.y = (int)b;
                gvk_push(w, e);
                break;
            case GUIW_EV_FOCUS: e.type = GVK_FOCUS; e.value = a != 0; gvk_push(w, e); break;
            case GUIW_EV_KEY:
            case GUIW_EV_BUTTON: e.type = GVK_KEY; e.code = (uint16_t)a; e.value = b != 0; gvk_push(w, e); break;
            case GUIW_EV_MOTION: e.type = GVK_MOTION; e.x = (int)a; e.y = (int)b; gvk_push(w, e); break;
            case GUIW_EV_RELATIVE_MOTION:
                if (a) { e.type = GVK_REL; e.code = 0; e.value = (int)a; gvk_push(w, e); }
                if (b) { e.type = GVK_REL; e.code = 1; e.value = (int)b; gvk_push(w, e); }
                break;
            case GUIW_EV_RESIZE: e.type = GVK_RESIZE; e.value = (int)a; e.y = (int)b; gvk_push(w, e); break;
            case GUIW_EV_CLOSE: e.type = GVK_CLOSE; gvk_push(w, e); break;
            case GUIW_EV_AXIS: e.type = GVK_WHEEL; e.value = (int)a; gvk_push(w, e); break;
            }
        } else if (m.object == w->frame_cb && m.opcode == GUIW_EV_DONE && na == 1) {
            w->frame_cb = 0;   // the compositor showed the frame the last commit asked about
        } else if (m.opcode == GUIW_EV_RELEASE && na == 0) {
            // a buffer the swapchain sent: the compositor let go of it
            w->releases++;
            nvk_constanos_surface_buffer_released((VkSurfaceKHR)w->surface_handle, m.object);
        }
    }
    memmove(w->rx, w->rx + off, w->rxlen - off);
    w->rxlen -= off;
}

// Waits up to `timeout_ms` (0: only what is there) for the compositor and handles every event that arrives. 0, or -EPIPE when it is gone.
static int gvk_pump(struct gvk_window *w, int timeout_ms) {
    for (;;) {
        if (w->closed) return -EPIPE;
        struct pollfd pf = { .fd = w->sock, .events = POLLIN };
        int r = poll(&pf, 1, timeout_ms);
        if (r < 0 && errno == EINTR) continue;
        if (r <= 0) return 0;
        if (w->rxlen == sizeof(w->rx)) { w->closed = 1; return -EPIPE; }
        long n = recv(w->sock, w->rx + w->rxlen, sizeof(w->rx) - w->rxlen, MSG_DONTWAIT);
        if (n == 0) { w->closed = 1; return -EPIPE; }
        if (n < 0) {
            if (errno == EAGAIN || errno == EINTR) return 0;
            w->closed = 1;
            return -EPIPE;
        }
        w->rxlen += (size_t)n;
        gvk_dispatch(w);
        timeout_ms = 0; // after the first read take only what is there
    }
}

// ── the hooks of constanos_vk_window.h ───────────────────────────────────

static uint32_t gvk_hook_new_id(void *user) {
    struct gvk_window *w = user;
    return w->next_id++;
}

static int gvk_hook_send_buffer(void *user, uint32_t id, int fd, uint64_t size_B, uint32_t width, uint32_t height, uint32_t pitch_B) {
    struct gvk_window *w = user;
    struct guiw_out o;
    memset(&o, 0, sizeof(o));
    guiw_create_gpu_buffer(&o, id, fd, (uint32_t)size_B, (int32_t)width, (int32_t)height, (int32_t)pitch_B, GUIW_FORMAT_XRGB8888);
    int r = gvk_send(w, &o);
    if (r == 0) w->buffers_sent++;
    return r;
}

// A present is paced by the compositor, as in Wayland: each commit asks for a `frame` callback, and the next one waits until it was answered (the
// compositor shows the frame, then says so). Without that a client that has free images presents as fast as it can and most of its frames are
// replaced before anyone sees them. A compositor that never answers (a window nobody draws) is waited for 100 ms at most.
#define GVK_THROTTLE_MS 100

static int gvk_hook_commit(void *user, uint32_t id) {
    struct gvk_window *w = user;
    if (w->frame_cb) {
        w->throttled++;
        for (int waited = 0; w->frame_cb && waited < GVK_THROTTLE_MS; waited += 5)
            if (gvk_pump(w, 5) < 0) return -EPIPE;
        if (w->frame_cb) { w->throttle_timeouts++; w->frame_cb = 0; }
    }
    struct guiw_out o;
    memset(&o, 0, sizeof(o));
    uint32_t cb = w->next_id++;
    guiw_frame(&o, w->surface_id, cb);
    guiw_attach(&o, w->surface_id, id);
    guiw_commit(&o, w->surface_id);
    int r = gvk_send(w, &o);
    if (r == 0) { w->commits++; w->frame_cb = cb; }
    return r;
}

static int gvk_hook_destroy_buffer(void *user, uint32_t id) {
    struct gvk_window *w = user;
    struct guiw_out o;
    memset(&o, 0, sizeof(o));
    guiw_destroy_buffer(&o, id);
    int r = gvk_send(w, &o);
    if (r == 0) w->buffers_destroyed++;
    return r;
}

static int gvk_hook_pump(void *user, int timeout_ms) {
    struct gvk_window *w = user;
    if (timeout_ms > 0) w->waits++; // an acquire that had every image held
    return gvk_pump(w, timeout_ms);
}

// Connects, makes the surface (object 2) and waits for the compositor's first `configure`. 0, or -1 (nothing listening, no answer in 5 s).
static int gvk_open(struct gvk_window *w, const char *title) {
    memset(w, 0, sizeof(*w));
    const char *path = getenv("GUI_DISPLAY");
    if (!path || !*path) path = "/tmp/gui-0";
    w->sock = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a;
    memset(&a, 0, sizeof(a));
    a.sun_family = AF_UNIX;
    strncpy(a.sun_path, path, sizeof(a.sun_path) - 1);
    if (w->sock < 0 || connect(w->sock, (struct sockaddr *)&a, sizeof(a)) < 0) {
        if (w->sock >= 0) close(w->sock);
        w->sock = -1;
        return -1;
    }
    w->surface_id = 2;
    w->next_id = 3;
    struct guiw_out o;
    memset(&o, 0, sizeof(o));
    guiw_create_surface(&o, w->surface_id);
    guiw_set_title(&o, w->surface_id, title);
    if (gvk_send(w, &o) < 0) return -1;
    for (int waited = 0; w->cfg_w == 0 && waited < 5000; waited += 50)
        if (gvk_pump(w, 50) < 0) return -1;
    return w->cfg_w ? 0 : -1;
}

// The VkSurfaceKHR of this window (NVK's nvk_constanos_surface_create, see constanos_vk_window.h).
static int gvk_surface_create(struct gvk_window *w, VkInstance instance, VkSurfaceKHR *surface) {
    w->hooks = (struct constanos_window){
        .user = w, .new_id = gvk_hook_new_id, .send_buffer = gvk_hook_send_buffer, .commit = gvk_hook_commit,
        .destroy_buffer = gvk_hook_destroy_buffer, .pump = gvk_hook_pump,
    };
    VkResult r = nvk_constanos_surface_create(instance, &w->hooks, surface);
    if (r == VK_SUCCESS) w->surface_handle = (uint64_t)*surface;
    return r == VK_SUCCESS ? 0 : -1;
}

// Says the window can take any size from min_w x min_h (maximize, resize drag, F11); `resize` events (GVK_RESIZE) then say which.
static int gvk_set_resizable(struct gvk_window *w, int min_w, int min_h) {
    struct guiw_out o;
    memset(&o, 0, sizeof(o));
    guiw_set_resizable(&o, w->surface_id, min_w, min_h);
    return gvk_send(w, &o);
}

static void gvk_close(struct gvk_window *w) {
    if (w->sock >= 0) close(w->sock);
    w->sock = -1;
}

#endif
