// constanos_gui_wire.h — the compositor's wire format, for C clients.
//
// The C half of `gui::wire` + `gui::protocol` (the Rust crate is the
// reference): a message is [object: u32][size << 16 | opcode: u32][args],
// native endian, `size` counting the whole message; a string is a u32
// length including its NUL, then the bytes padded to 4. An fd argument
// takes no bytes: it travels as SCM_RIGHTS beside the message.
//
// Pure — no syscalls — so the host can check it byte for byte against the
// Rust encoder and decoder (gui/tests/c_wire.rs compiles this header with
// the host's cc). Only what a client needs: encoding requests, splitting
// the incoming byte stream into events.
//
// Header-only (every function `static`), so fire, DOOM and Quake, each
// built by its own script, need nothing but an -I.

#ifndef CONSTANOS_GUI_WIRE_H
#define CONSTANOS_GUI_WIRE_H

#include <stddef.h>
#include <stdint.h>
#include <string.h>

#define GUIW_COMPOSITOR 1u
#define GUIW_FORMAT_XRGB8888 1u
#define GUIW_MAX_MESSAGE 4096u

// Event opcodes, by interface.
#define GUIW_EV_ERROR 0            // compositor: (object, code, string)
#define GUIW_EV_DELETE_ID 1        // compositor: (id)
#define GUIW_EV_RELEASE 0          // buffer: ()
#define GUIW_EV_CONFIGURE 0        // surface: (w, h)
#define GUIW_EV_FOCUS 1            // surface: (in)
#define GUIW_EV_KEY 2              // surface: (code, pressed)
#define GUIW_EV_MOTION 3           // surface: (x, y)
#define GUIW_EV_BUTTON 4           // surface: (code, pressed)
#define GUIW_EV_RELATIVE_MOTION 5  // surface: (dx, dy), dy positive down
#define GUIW_EV_DONE 0             // callback: (ms)

// ── Encoding ─────────────────────────────────────────────────────────────

struct guiw_out {
    uint8_t bytes[512];
    size_t len;
    int fds[4];
    int nfds;
    size_t start;   // of the message being built
    int overflow;   // set if anything did not fit; nothing is sent then
};

static void guiw_put(struct guiw_out *o, uint32_t w) {
    if (o->len + 4 > sizeof(o->bytes)) { o->overflow = 1; return; }
    memcpy(o->bytes + o->len, &w, 4);
    o->len += 4;
}

static void guiw_begin(struct guiw_out *o, uint32_t object, uint16_t opcode) {
    o->start = o->len;
    guiw_put(o, object);
    guiw_put(o, opcode); // size patched in by guiw_end
}

static void guiw_end(struct guiw_out *o) {
    if (o->overflow) return;
    uint32_t size = (uint32_t)(o->len - o->start), word;
    memcpy(&word, o->bytes + o->start + 4, 4);
    word |= size << 16;
    memcpy(o->bytes + o->start + 4, &word, 4);
}

static void guiw_string(struct guiw_out *o, const char *s) {
    uint32_t n = (uint32_t)strlen(s) + 1;
    guiw_put(o, n);
    uint32_t padded = (n + 3) & ~3u;
    if (o->len + padded > sizeof(o->bytes)) { o->overflow = 1; return; }
    memset(o->bytes + o->len, 0, padded);
    memcpy(o->bytes + o->len, s, n - 1);
    o->len += padded;
}

static void guiw_fd(struct guiw_out *o, int fd) {
    if (o->nfds >= 4) { o->overflow = 1; return; }
    o->fds[o->nfds++] = fd;
}

static void guiw_create_pool(struct guiw_out *o, uint32_t id, int fd, uint32_t size) {
    guiw_begin(o, GUIW_COMPOSITOR, 0); guiw_put(o, id); guiw_fd(o, fd); guiw_put(o, size); guiw_end(o);
}
static void guiw_create_surface(struct guiw_out *o, uint32_t id) {
    guiw_begin(o, GUIW_COMPOSITOR, 1); guiw_put(o, id); guiw_end(o);
}
static void guiw_create_buffer(struct guiw_out *o, uint32_t pool, uint32_t id, int32_t offset,
                               int32_t w, int32_t h, int32_t stride, uint32_t format) {
    guiw_begin(o, pool, 0);
    guiw_put(o, id); guiw_put(o, (uint32_t)offset); guiw_put(o, (uint32_t)w);
    guiw_put(o, (uint32_t)h); guiw_put(o, (uint32_t)stride); guiw_put(o, format);
    guiw_end(o);
}
static void guiw_attach(struct guiw_out *o, uint32_t surface, uint32_t buffer) {
    guiw_begin(o, surface, 0); guiw_put(o, buffer); guiw_end(o);
}
static void guiw_damage(struct guiw_out *o, uint32_t surface, int32_t x, int32_t y, int32_t w, int32_t h) {
    guiw_begin(o, surface, 1);
    guiw_put(o, (uint32_t)x); guiw_put(o, (uint32_t)y); guiw_put(o, (uint32_t)w); guiw_put(o, (uint32_t)h);
    guiw_end(o);
}
static void guiw_frame(struct guiw_out *o, uint32_t surface, uint32_t id) {
    guiw_begin(o, surface, 2); guiw_put(o, id); guiw_end(o);
}
static void guiw_commit(struct guiw_out *o, uint32_t surface) {
    guiw_begin(o, surface, 3); guiw_end(o);
}
static void guiw_set_title(struct guiw_out *o, uint32_t surface, const char *title) {
    guiw_begin(o, surface, 4); guiw_string(o, title); guiw_end(o);
}
static void guiw_lock_pointer(struct guiw_out *o, uint32_t surface, int on) {
    guiw_begin(o, surface, 6); guiw_put(o, on ? 1u : 0u); guiw_end(o);
}

// ── Decoding ─────────────────────────────────────────────────────────────

struct guiw_msg {
    uint32_t object;
    uint16_t opcode;
    uint16_t size;       // whole message, header included
    const uint8_t *args; // size - 8 bytes
};

// The first message in buf[0..len): returns its size and fills *m, 0 if
// it is not complete yet, -1 if the header is malformed (the stream is
// then unusable, as in gui::wire::Decoder).
static int guiw_next(const uint8_t *buf, size_t len, struct guiw_msg *m) {
    if (len < 8) return 0;
    uint32_t object, word;
    memcpy(&object, buf, 4);
    memcpy(&word, buf + 4, 4);
    uint32_t size = word >> 16;
    if (size < 8 || size % 4 != 0 || size > GUIW_MAX_MESSAGE) return -1;
    if (len < size) return 0;
    m->object = object;
    m->opcode = (uint16_t)(word & 0xFFFF);
    m->size = (uint16_t)size;
    m->args = buf + 8;
    return (int)size;
}

// Argument i (0-based) as a u32 / i32. The caller has checked the size.
static uint32_t guiw_arg(const struct guiw_msg *m, int i) {
    uint32_t v;
    memcpy(&v, m->args + 4 * i, 4);
    return v;
}

static int guiw_nargs(const struct guiw_msg *m) {
    return (m->size - 8) / 4;
}

#endif
