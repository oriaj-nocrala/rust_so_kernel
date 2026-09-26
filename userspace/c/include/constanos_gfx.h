// constanos_gfx.h — a picture and input for full-screen programs, in a
// window when there is a compositor and on the console when there is not.
//
// fire, DOOM and Quake draw a small 0x00RRGGBB frame and read evdev input.
// On the console that is FBIO_BLIT on /dev/fb (scaled by the kernel to the
// whole screen) plus /dev/input/event0 (grabbed) and event1. Under the
// compositor (which holds /dev/fb0, so FBIO_BLIT is EBUSY, and reads
// event0 itself) it is a window: `$GUI_DISPLAY` names the socket (set by
// the compositor for what it starts and by `term` for its shell, as
// WAYLAND_DISPLAY is), the frame is scaled by an integer factor into a
// shared-memory buffer, and input arrives as protocol events.
//
// Either way the program sees the same thing: gfx_present() of a frame and
// gfx_next_event() returning evdev-shaped events — EV_KEY with KEY_* and
// BTN_* codes, EV_REL with REL_X/REL_Y in the PS/2 sign convention (Y
// positive up) the ports were written against. In a window with
// GFX_MOUSE the pointer is locked to it (Ctrl+Alt lets go; a click takes
// it back).
//
// With GFX_HIDPI the program draws at full resolution instead: w x h is
// its size in logical pixels, gfx_scale() the factor it multiplies them
// by, and gfx_present() takes a (w * scale) x (h * scale) frame, shown
// pixel for pixel. Without it the frame is replicated by that factor —
// right for pixel art, blocky for antialiased text. Same as
// userspace::gfx::HIDPI.
//
// Header-only, like constanos_gui_wire.h. One window per process.

#ifndef CONSTANOS_GFX_H
#define CONSTANOS_GFX_H

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include "constanos_gui_wire.h"

#define GFX_MOUSE 1 // lock the pointer to the window and report motion
#define GFX_HIDPI 2 // the program draws at gfx_scale() itself

#define GFX_EV_KEY 1
#define GFX_EV_REL 2
#define GFX_REL_X 0
#define GFX_REL_Y 1

struct gfx_event {
    uint16_t type;  // GFX_EV_KEY / GFX_EV_REL (evdev's EV_KEY / EV_REL)
    uint16_t code;  // KEY_* / BTN_* / REL_X / REL_Y
    int32_t value;  // 1 press, 0 release / the motion
};

// ── State ────────────────────────────────────────────────────────────────

#define GFX_QUEUE 256

static struct {
    int windowed;
    int w, h;              // the program's frame, in logical pixels
    int dscale;            // GFX_HIDPI: the factor it draws at; 1 otherwise
    // console
    int fb, kbd, mouse;
    int blits;
    // window
    int sock;
    int scale;
    uint32_t *pool;        // (w * scale) x (h * scale)
    int busy;              // committed, not released yet
    uint8_t rx[8192];
    size_t rxlen;
    uint8_t held[64];      // key/button codes < 512 held down, for focus-out
    struct gfx_event q[GFX_QUEUE];
    unsigned qhead, qtail;
} gfx;

// ids of this client's objects
#define GFX_POOL 2u
#define GFX_BUFFER 3u
#define GFX_SURFACE 4u

struct gfx_blit_args {
    unsigned long ptr;
    unsigned int width;
    unsigned int height;
};
#define GFX_FBIO_BLIT 0x46420001UL
#define GFX_EVIOCGRAB 0x40044590UL
#define GFX_TIOCGWINSZ 0x5413UL

struct gfx_input_event { // the kernel's evdev record
    long tv_sec;
    long tv_usec;
    unsigned short type;
    unsigned short code;
    int value;
};

static void gfx_push(uint16_t type, uint16_t code, int32_t value) {
    if (gfx.qtail - gfx.qhead >= GFX_QUEUE) return; // full: drop, as evdev does
    struct gfx_event *e = &gfx.q[gfx.qtail++ % GFX_QUEUE];
    e->type = type; e->code = code; e->value = value;
}

static void gfx_note_key(uint32_t code, int pressed) {
    if (code < 512) {
        if (pressed) gfx.held[code / 8] |= (uint8_t)(1u << (code % 8));
        else gfx.held[code / 8] &= (uint8_t)~(1u << (code % 8));
    }
    gfx_push(GFX_EV_KEY, (uint16_t)code, pressed);
}

// ── Window ───────────────────────────────────────────────────────────────

static int gfx_send(struct guiw_out *o) {
    if (o->overflow) return -1;
    struct iovec iov = { o->bytes, o->len };
    char ctl[CMSG_SPACE(sizeof(int) * 4)];
    struct msghdr mh;
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
    long n = sendmsg(gfx.sock, &mh, 0);
    memset(o, 0, sizeof(*o));
    return n < 0 ? -1 : 0;
}

// The compositor went away: so does a program that only had its window.
static void gfx_gone(void) {
    fprintf(stderr, "gfx: compositor gone\n");
    exit(0);
}

// Turns every complete message in rx into state and events. Returns the
// configure size through *cw/*ch when one arrives.
static void gfx_dispatch(int *cw, int *ch) {
    size_t off = 0;
    for (;;) {
        struct guiw_msg m;
        int n = guiw_next(gfx.rx + off, gfx.rxlen - off, &m);
        if (n < 0) gfx_gone();
        if (n == 0) break;
        off += (size_t)n;
        int na = guiw_nargs(&m);
        if (m.object == GUIW_COMPOSITOR && m.opcode == GUIW_EV_ERROR && na >= 2) {
            fprintf(stderr, "gfx: protocol error %u on object %u\n", guiw_arg(&m, 1), guiw_arg(&m, 0));
            exit(1);
        } else if (m.object == GFX_BUFFER && m.opcode == GUIW_EV_RELEASE) {
            gfx.busy = 0;
        } else if (m.object == GFX_SURFACE && na >= 2) {
            uint32_t a = guiw_arg(&m, 0), b = guiw_arg(&m, 1);
            switch (m.opcode) {
            case GUIW_EV_CONFIGURE:
                if (cw) { *cw = (int)a; *ch = (int)b; }
                break;
            case GUIW_EV_KEY:
            case GUIW_EV_BUTTON:
                gfx_note_key(a, b != 0);
                break;
            case GUIW_EV_RELATIVE_MOTION:
                if (a) gfx_push(GFX_EV_REL, GFX_REL_X, (int32_t)a);
                if (b) gfx_push(GFX_EV_REL, GFX_REL_Y, -(int32_t)b); // PS/2: up positive
                break;
            }
        } else if (m.object == GFX_SURFACE && m.opcode == GUIW_EV_FOCUS && na >= 1 && guiw_arg(&m, 0) == 0) {
            // The releases of keys held now will go to another window.
            for (uint32_t code = 0; code < 512; code++)
                if (gfx.held[code / 8] & (1u << (code % 8))) gfx_note_key(code, 0);
        }
    }
    memmove(gfx.rx, gfx.rx + off, gfx.rxlen - off);
    gfx.rxlen -= off;
}

// Reads what the socket has; blocks for more if `wait`.
static void gfx_pump(int wait, int *cw, int *ch) {
    do {
        if (gfx.rxlen == sizeof(gfx.rx)) gfx_gone();
        long n = recv(gfx.sock, gfx.rx + gfx.rxlen, sizeof(gfx.rx) - gfx.rxlen, wait ? 0 : MSG_DONTWAIT);
        if (n == 0) gfx_gone();
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR) break;
            gfx_gone();
        }
        gfx.rxlen += (size_t)n;
        gfx_dispatch(cw, ch);
        wait = 0; // after the first read, take only what is there
    } while (1);
}

static int gfx_open_window(const char *path, const char *title, int flags) {
    gfx.sock = socket(AF_UNIX, SOCK_STREAM, 0);
    struct sockaddr_un a;
    memset(&a, 0, sizeof(a));
    a.sun_family = AF_UNIX;
    strncpy(a.sun_path, path, sizeof(a.sun_path) - 1);
    if (gfx.sock < 0 || connect(gfx.sock, (struct sockaddr *)&a, sizeof(a)) < 0) {
        if (gfx.sock >= 0) close(gfx.sock);
        return -1;
    }

    struct guiw_out o;
    memset(&o, 0, sizeof(o));
    guiw_create_surface(&o, GFX_SURFACE);
    if (gfx_send(&o) < 0) return -1;
    int half_w = 0, half_h = 0;
    while (half_w == 0) gfx_pump(1, &half_w, &half_h);

    // The compositor suggests half the screen. The largest integer scale
    // up to 3 that fits with the title bar and the cascade offset.
    int sw = half_w * 2, sh = half_h * 2;
    int s = 3;
    while (s > 1 && (gfx.w * s > sw - 40 || gfx.h * s > sh - 60)) s--;
    gfx.scale = s;
    int pw = gfx.w * s, ph = gfx.h * s;
    size_t size = (size_t)pw * ph * 4;

    int mfd = memfd_create("gfx", 0);
    if (mfd < 0 || ftruncate(mfd, (off_t)size) < 0) return -1;
    void *p = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, mfd, 0);
    if (p == MAP_FAILED) return -1;
    gfx.pool = (uint32_t *)p;

    guiw_create_pool(&o, GFX_POOL, mfd, (uint32_t)size);
    guiw_create_buffer(&o, GFX_POOL, GFX_BUFFER, 0, pw, ph, pw * 4, GUIW_FORMAT_XRGB8888);
    guiw_set_title(&o, GFX_SURFACE, title);
    if (flags & GFX_MOUSE) guiw_lock_pointer(&o, GFX_SURFACE, 1);
    int r = gfx_send(&o);
    close(mfd); // the compositor has its own now
    return r;
}

static void gfx_present_window(const uint32_t *px) {
    while (gfx.busy) gfx_pump(1, NULL, NULL);
    // The frame is (w * dscale) x (h * dscale), replicated s times.
    int fw = gfx.w * gfx.dscale, fh = gfx.h * gfx.dscale;
    int s = gfx.scale / gfx.dscale, pw = fw * s;
    for (int y = 0; y < fh; y++) {
        uint32_t *row = gfx.pool + (size_t)y * s * pw;
        const uint32_t *src = px + (size_t)y * fw;
        if (s == 1) {
            memcpy(row, src, (size_t)fw * 4);
            continue;
        }
        for (int x = 0; x < fw; x++)
            for (int k = 0; k < s; k++) row[x * s + k] = src[x];
        for (int k = 1; k < s; k++) memcpy(row + (size_t)k * pw, row, (size_t)pw * 4);
    }
    struct guiw_out o;
    memset(&o, 0, sizeof(o));
    guiw_attach(&o, GFX_SURFACE, GFX_BUFFER);
    guiw_damage(&o, GFX_SURFACE, 0, 0, pw, fh * s);
    guiw_commit(&o, GFX_SURFACE);
    if (gfx_send(&o) < 0) gfx_gone();
    gfx.busy = 1;
}

// ── Console ──────────────────────────────────────────────────────────────

static int gfx_open_console(void) {
    gfx.fb = open("/dev/fb", O_WRONLY);
    if (gfx.fb < 0) return -1;
    // EVIOCGRAB: keys go to us, not to the tty -- otherwise everything
    // typed (arrows, the `y` of DOOM's "quit?") is replayed by the shell
    // afterwards. The kernel drops the grab when the fd closes, so a crash
    // cannot leave the keyboard grabbed.
    gfx.kbd = open("/dev/input/event0", O_RDONLY);
    if (gfx.kbd >= 0) ioctl(gfx.kbd, GFX_EVIOCGRAB, 1);
    gfx.mouse = open("/dev/input/event1", O_RDONLY);
    // Drop the backlog: the ring fills from every key since boot, and the
    // Enter that started us is still in it.
    struct gfx_input_event ev;
    while (gfx.kbd >= 0 && read(gfx.kbd, &ev, sizeof(ev)) == (long)sizeof(ev)) { }
    while (gfx.mouse >= 0 && read(gfx.mouse, &ev, sizeof(ev)) == (long)sizeof(ev)) { }
    return 0;
}

// The factor FBIO_BLIT would scale a w x h frame by: the largest integer
// that fits the screen, whose size in pixels TIOCGWINSZ gives. 1 if the
// kernel does not say.
static int gfx_console_scale(void) {
    unsigned short ws[4] = { 0, 0, 0, 0 }; // row, col, xpixel, ypixel
    if (ioctl(gfx.fb, GFX_TIOCGWINSZ, ws) != 0 || gfx.w <= 0 || gfx.h <= 0) return 1;
    int sx = ws[2] / gfx.w, sy = ws[3] / gfx.h;
    int s = sx < sy ? sx : sy;
    return s < 1 ? 1 : s;
}

static void gfx_present_console(const uint32_t *px) {
    struct gfx_blit_args args = { (unsigned long)px, (unsigned)(gfx.w * gfx.dscale), (unsigned)(gfx.h * gfx.dscale) };
    if (ioctl(gfx.fb, GFX_FBIO_BLIT, &args) < 0 && errno == EBUSY && gfx.blits == 0) {
        fprintf(stderr, "gfx: the screen belongs to a compositor, and GUI_DISPLAY is not set\n");
        exit(1);
    }
    gfx.blits++;
}

// ── API ──────────────────────────────────────────────────────────────────

// A w x h frame. A window if $GUI_DISPLAY names a compositor that
// answers, the console otherwise. Returns 0, or -1 with nothing to draw on.
static int gfx_open(const char *title, int w, int h, int flags) {
    memset(&gfx, 0, sizeof(gfx));
    gfx.w = w;
    gfx.h = h;
    gfx.dscale = 1;
    gfx.fb = gfx.kbd = gfx.mouse = gfx.sock = -1;
    const char *d = getenv("GUI_DISPLAY");
    if (d && *d) {
        if (gfx_open_window(d, title, flags) == 0) {
            gfx.windowed = 1;
            if (flags & GFX_HIDPI) gfx.dscale = gfx.scale;
            return 0;
        }
        fprintf(stderr, "gfx: no compositor at %s, using the console\n", d);
    }
    if (gfx_open_console() < 0) return -1;
    if (flags & GFX_HIDPI) gfx.dscale = gfx_console_scale();
    return 0;
}

// The factor a GFX_HIDPI program draws at (1 without the flag): its frame
// is (w * gfx_scale()) x (h * gfx_scale()).
static int gfx_scale(void) {
    return gfx.dscale;
}

static int gfx_windowed(void) {
    return gfx.windowed;
}

// Shows a w x h frame of 0x00RRGGBB pixels ((w * gfx_scale()) x
// (h * gfx_scale()) with GFX_HIDPI). In a window, waits for the
// compositor to have taken the previous one.
static void gfx_present(const uint32_t *px) {
    if (gfx.windowed) gfx_present_window(px);
    else gfx_present_console(px);
}

// The next input event, without blocking: 1 if *e was filled.
static int gfx_next_event(struct gfx_event *e) {
    if (gfx.windowed) {
        if (gfx.qhead == gfx.qtail) gfx_pump(0, NULL, NULL);
        if (gfx.qhead == gfx.qtail) return 0;
        *e = gfx.q[gfx.qhead++ % GFX_QUEUE];
        return 1;
    }
    struct gfx_input_event ev;
    int fds[2] = { gfx.kbd, gfx.mouse };
    for (int i = 0; i < 2; i++) {
        while (fds[i] >= 0 && read(fds[i], &ev, sizeof(ev)) == (long)sizeof(ev)) {
            if (ev.type != GFX_EV_KEY && ev.type != GFX_EV_REL) continue; // EV_SYN
            e->type = ev.type;
            e->code = ev.code;
            e->value = ev.value;
            return 1;
        }
    }
    return 0;
}

static void gfx_close(void) {
    if (gfx.kbd >= 0) {
        ioctl(gfx.kbd, GFX_EVIOCGRAB, 0);
        close(gfx.kbd);
    }
    if (gfx.mouse >= 0) close(gfx.mouse);
    if (gfx.fb >= 0) close(gfx.fb);
    if (gfx.sock >= 0) close(gfx.sock); // the window goes with it
    gfx.kbd = gfx.mouse = gfx.fb = gfx.sock = -1;
}

#endif
