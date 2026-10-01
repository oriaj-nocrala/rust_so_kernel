// /dev/nvgpu on the hardware (G4c): the same interface nvgpu_sw_test.c checks against the software device, but with a GPU behind it. Real
// compute launches through EXEC, every result read back by a path other than the writer's:
//   - fill (a shader that stores 256 distinct words) into system memory, read by the CPU; the other 768 words of the page must stay the
//     scribble they started with;
//   - fill into VRAM, read back by the GPU (a second shader copies the page into system memory) in the same EXEC: two push segments;
//   - rebinding: a VA moved from one buffer to another (system and VRAM) must reach the new buffer, never the old one (the TLB flush);
//   - the ring: 700 submissions (the ring has 1024 entries, the kernel keeps 64 fences in flight) and 32 real launches in flight together;
//   - the device handed back: close and reopen, and a holder that dies with work in flight.
//   - a buffer shared between two processes (G5 layer 2): a system region and a VRAM page exported as descriptors, sent by SCM_RIGHTS and
//     imported by a child in its own session; each process's GPU writes into the same pages through its own VA and the other side reads them;
//   - a timeline shared between two processes (explicit sync): each side queues GPU work that signals it and then sits idle (blocked on a
//     socket, no GPU call) while the other waits on it and reads what that work wrote: the kernel resolves the pending fences for whoever asks;
//   - several clients at once (G5 layer 1): four processes, each its own session (its own slice of the VA space), each with its own compute
//     context, launching at the same time; every output checked, every session's range distinct.
// On the software device (QEMU: no GPU) the execution checks are skipped and only the interface is exercised.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include "nvgpu.h"
#include "nvgpu_qmd.h"
#include "nvgpu_shaders.h"

extern int ioctl(int fd, unsigned long request, ...);

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf(" [errno %d]\n", errno); } } while (0)
// A check without which nothing after it means anything: end the run.
#define REQUIRE(cond, ...) do { if (!(cond)) { failures++; printf("  FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf(" [errno %d]\n", errno); printf("nvgpu_hw_test: stopped, %d failure(s)\n", failures); return 1; } else printf("  ok   %s\n", #cond); } while (0)
// A check about what the GPU did: meaningless on the software device.
#define HWCHECK(cond, ...) do { if (hw) CHECK(cond, __VA_ARGS__); else printf("  skip %s (software device)\n", #cond); } while (0)

static int fd = -1;
static int hw;

static int call(unsigned long req, void *arg) { errno = 0; return ioctl(fd, req, arg); }

static int64_t now_ms(void) {
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec * 1000ll + ts.tv_nsec / 1000000;
}

static int64_t now_us(void) {
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec * 1000000ll + ts.tv_nsec / 1000;
}

static void nap_us(long us) {
   struct timespec ts = { 0, us * 1000 };
   nanosleep(&ts, NULL);
}

// ---- thin wrappers over the ioctls -----------------------------------------------------------------------------------------------

static uint32_t bo_create(uint64_t size, uint32_t flags, uint64_t *off) {
   struct nvg_bo_create c = { .size = size, .flags = flags };
   if (call(NVG_IOC_BO_CREATE, &c) != 0) return 0;
   if (off) *off = c.mmap_offset;
   return c.handle;
}

static uint64_t va_alloc(uint64_t size, uint64_t align) {
   struct nvg_va_alloc a = { .size = size, .align = align };
   return call(NVG_IOC_VA_ALLOC, &a) == 0 ? a.va : 0;
}

static int va_bind_call(uint64_t va, uint64_t size, uint32_t handle, uint64_t bo_off) {
   struct nvg_va_bind b = { .va = va, .size = size, .bo_offset = bo_off, .handle = handle };
   return call(NVG_IOC_VA_BIND, &b);
}

static int unbind(uint64_t va, uint64_t size) {
   struct nvg_va_unbind u = { .va = va, .size = size };
   return call(NVG_IOC_VA_UNBIND, &u);
}

static uint32_t sync_create(uint64_t initial) {
   struct nvg_sync_create s = { .initial = initial };
   return call(NVG_IOC_SYNC_CREATE, &s) == 0 ? s.handle : 0;
}

static int sync_value(uint32_t h, uint64_t *v) {
   struct nvg_sync_query q = { .handle = h };
   int r = call(NVG_IOC_SYNC_QUERY, &q);
   *v = q.value;
   return r;
}

static uint32_t ctx_create(uint32_t engines) {
   struct nvg_ctx_create c = { .engines = engines };
   return call(NVG_IOC_CTX_CREATE, &c) == 0 ? c.ctx : 0;
}

static unsigned long eagains;   // EXECs the kernel answered with EAGAIN (ring or fence slots full) before accepting them

static int ctx_destroy(uint32_t ctx) {
   struct nvg_ctx_destroy d = { .ctx = ctx };
   return call(NVG_IOC_CTX_DESTROY, &d);
}

/// EXEC signalling `sync` = `value` when its pushes and the kernel's fence have run; retried while the ring is full (EAGAIN).
static int exec_signal(uint32_t ctx, const struct nvg_push *p, uint32_t np, uint32_t sync, uint64_t value) {
   struct nvg_sync_ref sig = { .handle = sync, .value = value };
   struct nvg_exec e = { .ctx = ctx, .push_count = np, .sig_count = 1, .pushes = (uintptr_t)p, .signals = (uintptr_t)&sig };
   int64_t deadline = now_ms() + 20000;
   for (;;) {
      int r = call(NVG_IOC_EXEC, &e);
      if (r == 0 || errno != EAGAIN) return r;
      eagains++;
      if (now_ms() > deadline) { errno = ETIMEDOUT; return -1; }
      nap_us(50);
   }
}

/// Wait (by sleeping and asking again: the kernel never blocks in an ioctl) until timeline `h` reaches `value`.
static int wait_timeline(uint32_t h, uint64_t value, int timeout_ms) {
   struct nvg_sync_ref ref = { .handle = h, .value = value };
   struct nvg_sync_wait w = { .refs = (uintptr_t)&ref, .count = 1 };
   int64_t deadline = now_ms() + timeout_ms;
   for (;;) {
      int r = call(NVG_IOC_SYNC_WAIT, &w);
      if (r == 0 || errno != EAGAIN) return r;
      if (now_ms() > deadline) { errno = ETIMEDOUT; return -1; }
      nap_us(20);
   }
}

// ---- a region of system memory the GPU sees --------------------------------------------------------------------------------------

struct region {
   uint32_t bo;
   uint8_t *cpu;
   uint64_t va;
   uint64_t size;
};

static int region_make(struct region *r, uint64_t size) {
   uint64_t off = 0;
   memset(r, 0, sizeof *r);
   r->bo = bo_create(size, NVG_BO_SYSTEM, &off);
   if (!r->bo) return -1;
   r->size = size;
   r->cpu = mmap(NULL, size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, off);
   if (r->cpu == MAP_FAILED) return -1;
   r->va = va_alloc(size, 0x10000);
   if (!r->va) return -1;
   return va_bind_call(r->va, size, r->bo, 0);
}

// ---- sharing BOs: descriptors by SCM_RIGHTS, imports ---------------------------------------------------------------------------------

static int send_fd(int sock, int what) {
   char data = 'x';
   struct iovec iov = { .iov_base = &data, .iov_len = 1 };
   char control[CMSG_SPACE(sizeof(int))];
   memset(control, 0, sizeof control);
   struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = control, .msg_controllen = sizeof control };
   struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
   c->cmsg_level = SOL_SOCKET;
   c->cmsg_type = SCM_RIGHTS;
   c->cmsg_len = CMSG_LEN(sizeof(int));
   memcpy(CMSG_DATA(c), &what, sizeof(int));
   return sendmsg(sock, &msg, 0) == 1 ? 0 : -1;
}

static int recv_fd(int sock) {
   char data = 0;
   struct iovec iov = { .iov_base = &data, .iov_len = 1 };
   char control[CMSG_SPACE(sizeof(int))];
   memset(control, 0, sizeof control);
   struct msghdr msg = { .msg_iov = &iov, .msg_iovlen = 1, .msg_control = control, .msg_controllen = sizeof control };
   if (recvmsg(sock, &msg, 0) != 1) return -1;
   struct cmsghdr *c = CMSG_FIRSTHDR(&msg);
   int got = -1;
   if (c && c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) memcpy(&got, CMSG_DATA(c), sizeof(int));
   return got;
}

static int sync_export(uint32_t handle) {
   struct nvg_sync_export e = { .handle = handle };
   return call(NVG_IOC_SYNC_EXPORT, &e);
}

static uint32_t sync_import(int sfd) {
   struct nvg_sync_import im = { .fd = sfd };
   return call(NVG_IOC_SYNC_IMPORT, &im) == 0 ? im.handle : 0;
}

static int bo_export(uint32_t handle) {
   struct nvg_bo_export e = { .handle = handle };
   return call(NVG_IOC_BO_EXPORT, &e);
}

/// Import the BO behind descriptor `bofd` into the current session and make a region of it: mapped, at a VA of this session's own, bound.
static int region_import(struct region *r, int bofd) {
   struct nvg_bo_import im = { .fd = bofd };
   memset(r, 0, sizeof *r);
   if (call(NVG_IOC_BO_IMPORT, &im) != 0) return -1;
   r->bo = im.handle;
   r->size = im.size_out;
   if (im.mmap_offset != ~0ull) {
      r->cpu = mmap(NULL, r->size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, im.mmap_offset);
      if (r->cpu == MAP_FAILED) return -1;
   }
   r->va = va_alloc(r->size, 0x10000);
   if (!r->va) return -1;
   return va_bind_call(r->va, r->size, r->bo, 0);
}

// ---- compute launches ------------------------------------------------------------------------------------------------------------

// A launch "slot" is 16 KiB of a region: the shader, the QMD, constant buffer 0 and the push buffer in its first page, the output page,
// then a page with the grid's own semaphore at +0x10. The layout is nvgpu::gr::KERN_* with room for the push.
#define SLOT_BYTES 0x4000
#define OFF_SHADER 0x0
#define OFF_QMD 0x400
#define OFF_CB0 0x800
#define OFF_PUSH 0xc00
#define OFF_OUT 0x1000
#define OFF_SEM 0x2000
#define SEM_GRID 0x10
#define CBUF0_BYTES 0x200
#define PARAM0 0x160
#define PARAM1 0x168

static uint32_t fill_word(uint32_t i) { return i * 0x9e3779b1u ^ 0xc0de0000u; }
static uint32_t fillwt_word(uint32_t i) { return fill_word(i) ^ 0x12345678u; }
static uint32_t scribble(uint32_t i) { return 0x5c21b000u | (i & 0xfff); }

static uint32_t hdr(uint32_t subch, uint32_t method, uint32_t count) { return (1u << 29) | (count << 16) | (subch << 13) | (method >> 2); }

/// The methods of a compute launch on subchannel 1 (nvgpu::gr::dispatch_push without its own fence: the kernel appends one): bind the
/// object, the shared and local memory windows, invalidate the scheduler caches, name the QMD, schedule it, wait for the engine to go idle.
static uint32_t launch_push(uint32_t *w, uint64_t qmd_va) {
   uint32_t n = 0;
   w[n++] = hdr(1, 0x000, 1); w[n++] = 0xc7c0;                    // SET_OBJECT AMPERE_COMPUTE_B
   w[n++] = hdr(1, 0x2a0, 2); w[n++] = 0; w[n++] = 0xfe000000;    // SET_SHADER_SHARED_MEMORY_WINDOW
   w[n++] = hdr(1, 0x7b0, 2); w[n++] = 0; w[n++] = 0xff000000;    // SET_SHADER_LOCAL_MEMORY_WINDOW
   w[n++] = hdr(1, 0x298, 1); w[n++] = 0;                         // INVALIDATE_SKED_CACHES
   w[n++] = hdr(1, 0x2b4, 1); w[n++] = (uint32_t)(qmd_va >> 8);   // SEND_PCAS_A
   w[n++] = hdr(1, 0x2c0, 1); w[n++] = 3;                         // SEND_SIGNALING_PCAS2_B: INVALIDATE_COPY_SCHEDULE
   w[n++] = hdr(1, 0x110, 1); w[n++] = 0;                         // WAIT_FOR_IDLE
   return n;
}

/// Methods of a linear copy on the copy subchannel (4), as NVK's upload queue pushes them: no SET_OBJECT, the kernel binds the class.
static uint32_t copy_push(uint32_t *w, uint64_t src, uint64_t dst, uint32_t bytes) {
   uint32_t n = 0;
   w[n++] = hdr(4, 0x400, 4); w[n++] = src >> 32; w[n++] = (uint32_t)src; w[n++] = dst >> 32; w[n++] = (uint32_t)dst;  // OFFSET_IN/OUT
   w[n++] = hdr(4, 0x410, 4); w[n++] = bytes; w[n++] = bytes; w[n++] = bytes; w[n++] = 1;      // PITCH_IN/OUT, LINE_LENGTH_IN, LINE_COUNT
   w[n++] = hdr(4, 0x300, 1); w[n++] = 2 | (1 << 2) | (1 << 7) | (1 << 8);                     // LAUNCH_DMA: non-pipelined, flush, pitch layouts
   return n;
}

/// Set up slot `slot` of `r` to run `shader` (`shader_bytes` of SASS, `registers` of them; `launch_prepare`: 384 bytes, 8) as 8 CTAs of 32 threads with kernel parameters `p0` and `p1`, and
/// scribble its output page. Returns the push segment to EXEC.
static struct nvg_push launch_prepare_ex(struct region *r, unsigned slot, const uint8_t *shader, unsigned shader_bytes, unsigned registers, uint64_t p0, uint64_t p1, uint32_t release_payload) {
   uint8_t *cpu = r->cpu + (uint64_t)slot * SLOT_BYTES;
   uint64_t va = r->va + (uint64_t)slot * SLOT_BYTES;
   memcpy(cpu + OFF_SHADER, shader, shader_bytes);
   memset(cpu + OFF_CB0, 0, CBUF0_BYTES);
   memcpy(cpu + OFF_CB0 + PARAM0, &p0, 8);
   memcpy(cpu + OFF_CB0 + PARAM1, &p1, 8);
   struct nvg_qmd_launch l = {
      .program = va + OFF_SHADER, .registers = registers, .grid = { 8, 1, 1 }, .block = { 32, 1, 1 },
      .cbuf0 = va + OFF_CB0, .cbuf0_size = CBUF0_BYTES, .release = va + OFF_SEM + SEM_GRID, .release_payload = release_payload,
   };
   uint32_t q[64];
   nvg_qmd_build(q, &l);
   memcpy(cpu + OFF_QMD, q, 256);
   uint32_t w[32];
   uint32_t n = launch_push(w, va + OFF_QMD);
   memcpy(cpu + OFF_PUSH, w, n * 4);
   uint32_t *out = (uint32_t *)(cpu + OFF_OUT);
   for (uint32_t i = 0; i < 1024; i++) out[i] = scribble(i);
   *(volatile uint32_t *)(cpu + OFF_SEM + SEM_GRID) = 0;
   return (struct nvg_push){ .va = va + OFF_PUSH, .bytes = n * 4, .flags = 0 };
}

static struct nvg_push launch_prepare(struct region *r, unsigned slot, const uint8_t *shader, uint64_t p0, uint64_t p1, uint32_t release_payload) {
   return launch_prepare_ex(r, slot, shader, 384, 8, p0, p1, release_payload);
}

static uint32_t *slot_out(struct region *r, unsigned slot) { return (uint32_t *)(r->cpu + (uint64_t)slot * SLOT_BYTES + OFF_OUT); }
static uint64_t slot_out_va(struct region *r, unsigned slot) { return r->va + (uint64_t)slot * SLOT_BYTES + OFF_OUT; }
static uint32_t slot_grid_sem(struct region *r, unsigned slot) { return *(volatile uint32_t *)(r->cpu + (uint64_t)slot * SLOT_BYTES + OFF_SEM + SEM_GRID); }

/// Does the slot's output page hold `want(i)` for the 256 words the grid writes, and the scribble for the other 768?
static int out_is(struct region *r, unsigned slot, uint32_t (*want)(uint32_t), int *first_bad) {
   const uint32_t *out = slot_out(r, slot);
   for (uint32_t i = 0; i < 1024; i++) {
      uint32_t w = i < 256 ? want(i) : scribble(i);
      if (out[i] != w) { *first_bad = i; return 0; }
   }
   return 1;
}

/// One client of the several-sessions test, in whatever session `fd` is: its own context, timeline and region; `rounds` launches, each
/// read back and checked (clients with an odd `tag` run the other shader, so a mixed-up page shows). `*va0` is the session's first VA.
/// Returns 0, or the number of the step that failed.
static int client(int tag, int rounds, uint64_t *va0) {
   struct nvg_info inf;
   if (call(NVG_IOC_INFO, &inf) != 0) return 1;
   *va0 = inf.va_start;
   uint32_t c = ctx_create(NVG_ENGINE_3D | NVG_ENGINE_COMPUTE | NVG_ENGINE_COPY), t = sync_create(0);
   if (!c || !t) return 2;
   struct region r;
   if (region_make(&r, SLOT_BYTES) != 0) return 3;
   if (r.va < inf.va_start || r.va + SLOT_BYTES > inf.va_end) return 4;
   const uint8_t *sh = (tag & 1) ? nvg_shader_fillwt : nvg_shader_fill;
   uint32_t (*want)(uint32_t) = (tag & 1) ? fillwt_word : fill_word;
   for (int i = 0; i < rounds; i++) {
      struct nvg_push q = launch_prepare(&r, 0, sh, slot_out_va(&r, 0), 0, 0x700 + i);
      if (exec_signal(c, &q, 1, t, i + 1) != 0) return 5;
      if (wait_timeline(t, i + 1, 10000) != 0) return 6;
      int first;
      if (hw && !out_is(&r, 0, want, &first)) return 7;
   }
   return ctx_destroy(c) == 0 ? 0 : 8;
}

/// `nvgpu_hw_test grcopy[-bind]` (an experiment, run last in the metal job: a copy push the GR channel cannot take faults it for good): NVK's
/// graphics queue pushes image copies on subchannel 4 of the GR channel. Does a copy host -> VRAM -> host through a compute + copy context
/// (the GR channel), with nothing binding the copy class (`grcopy`) or with a SET_OBJECT of it in the push first (`grcopy-bind`).
static int grcopy(int with_bind) {
   printf("nvgpu_hw_test %s:\n", with_bind ? "grcopy-bind" : "grcopy");
   fd = open("/dev/nvgpu", O_RDWR);
   CHECK(fd >= 0, "open");
   if (fd < 0) return 1;
   struct nvg_info info;
   call(NVG_IOC_INFO, &info);
   hw = !(info.flags & NVG_INFO_SOFTWARE);
   uint32_t ctx = ctx_create(NVG_ENGINE_COMPUTE | NVG_ENGINE_COPY), tl = sync_create(0);
   struct region r;
   CHECK(ctx && tl && region_make(&r, 4 * SLOT_BYTES) == 0, "a compute + copy context and a region");
   uint32_t vb = bo_create(4096, NVG_BO_VRAM, NULL);
   uint64_t vva = va_alloc(4096, 4096);
   CHECK(vb && vva && va_bind_call(vva, 4096, vb, 0) == 0, "a VRAM page");
   uint32_t *src = (uint32_t *)(r.cpu + OFF_OUT), *dst = (uint32_t *)(r.cpu + SLOT_BYTES + OFF_OUT);
   for (uint32_t i = 0; i < 1024; i++) { src[i] = fill_word(i) ^ 0x0c0c0c0cu; dst[i] = scribble(i); }
   uint32_t w[40], n = 0;
   if (with_bind) { w[n++] = hdr(4, 0, 1); w[n++] = 0xc7b5; }
   n += copy_push(w + n, r.va + OFF_OUT, vva, 4096);
   n += copy_push(w + n, vva, r.va + SLOT_BYTES + OFF_OUT, 4096);
   memcpy(r.cpu + OFF_PUSH, w, n * 4);
   struct nvg_push p = { .va = r.va + OFF_PUSH, .bytes = n * 4, .flags = 0 };
   CHECK(exec_signal(ctx, &p, 1, tl, 1) == 0, "EXEC of a copy on the GR channel");
   int ok = wait_timeline(tl, 1, 15000) == 0;
   int same = 1;
   for (uint32_t i = 0; i < 1024; i++) same &= dst[i] == src[i];
   HWCHECK(ok && same, "the copy ran on the GR channel (fence %s, data %s)", ok ? "came" : "never came", same ? "intact" : "not copied");
   printf("nvgpu_hw_test grcopy: %d failure(s)\n", failures);
   return failures ? 1 : 0;
}

/// `nvgpu_hw_test rc` (an experiment for the end of the metal job: it kills the GPU channel on purpose): a launch whose program address
/// is a VA nothing is bound at. The GPU takes an MMU fault, RM resets the channel (RC_TRIGGERED) and it never releases the fence. The kernel
/// must notice through RM's event (which names the channel) within tens of milliseconds, not the 10 s hang limit, report that channel's
/// fences as done so the waiter wakes, answer the next EXEC on it with EIO, and leave every other context (and new ones) alone.
static int rc_test(void) {
   printf("nvgpu_hw_test rc:\n");
   fd = open("/dev/nvgpu", O_RDWR);
   CHECK(fd >= 0, "open");
   if (fd < 0) return 1;
   struct nvg_info info;
   call(NVG_IOC_INFO, &info);
   hw = !(info.flags & NVG_INFO_SOFTWARE);
   // two contexts: each has a channel of its own (the first takes the boot's, the second is made here)
   uint32_t ca = ctx_create(NVG_ENGINE_COMPUTE), cb = ctx_create(NVG_ENGINE_COMPUTE), tl = sync_create(0), tb = sync_create(0);
   struct region r;
   REQUIRE(ca && cb && tl && tb && region_make(&r, 8 * SLOT_BYTES) == 0, "two compute contexts (%u %u), two timelines and a region", ca, cb);
   int first = 0;
   // B works before the fault
   struct nvg_push pb0 = launch_prepare(&r, 2, nvg_shader_fill, slot_out_va(&r, 2), 0, 3);
   CHECK(exec_signal(cb, &pb0, 1, tb, 1) == 0 && wait_timeline(tb, 1, 5000) == 0, "B runs a launch before the fault");
   HWCHECK(out_is(&r, 2, fill_word, &first), "and its output is right (first bad %d)", first);
   // A faults: a launch whose program is a VA nothing is bound at
   uint64_t bad = va_alloc(0x10000, 0x10000);   // allocated, never bound
   CHECK(bad != 0, "a VA range nothing is bound at");
   struct nvg_push p = launch_prepare(&r, 0, nvg_shader_fill, r.va + OFF_OUT, 0, 1);
   struct nvg_qmd_launch l = { .program = bad, .registers = 8, .grid = { 8, 1, 1 }, .block = { 32, 1, 1 }, .cbuf0 = r.va + OFF_CB0, .cbuf0_size = CBUF0_BYTES };
   uint32_t q[64];
   nvg_qmd_build(q, &l);
   memcpy(r.cpu + OFF_QMD, q, 256);
   int64_t t0 = now_ms();
   CHECK(exec_signal(ca, &p, 1, tl, 1) == 0, "EXEC of the faulting launch on A is accepted");
   int woke = wait_timeline(tl, 1, 20000) == 0;
   int64_t ms = now_ms() - t0;
   printf("  the waiter woke after %lld ms\n", (long long)ms);
   HWCHECK(woke && ms < 3000, "the fault was noticed and A's fence released in %lld ms (the hang limit is 10000)", (long long)ms);
   struct nvg_push q2 = launch_prepare(&r, 1, nvg_shader_fill, slot_out_va(&r, 1), 0, 2);
   int rr = exec_signal(ca, &q2, 1, tl, 2);
   HWCHECK(rr < 0 && errno == EIO, "the next EXEC on A says EIO (%d, errno %d)", rr, errno);
   // the fault killed A's channel only: B goes on, and a new context gets a fresh channel
   struct nvg_push pb1 = launch_prepare(&r, 3, nvg_shader_fill, slot_out_va(&r, 3), 0, 4);
   CHECK(exec_signal(cb, &pb1, 1, tb, 2) == 0 && wait_timeline(tb, 2, 5000) == 0, "B still runs a launch after A's fault");
   HWCHECK(out_is(&r, 3, fill_word, &first), "B's output is right (first bad %d)", first);
   uint32_t cc = ctx_create(NVG_ENGINE_COMPUTE), tc = sync_create(0);
   CHECK(cc != 0 && tc != 0, "a new context after the fault");
   struct nvg_push pc = launch_prepare(&r, 4, nvg_shader_fill, slot_out_va(&r, 4), 0, 5);
   CHECK(exec_signal(cc, &pc, 1, tc, 1) == 0 && wait_timeline(tc, 1, 5000) == 0, "C runs a launch on its fresh channel");
   HWCHECK(out_is(&r, 4, fill_word, &first), "C's output is right (first bad %d)", first);
   CHECK(ctx_destroy(ca) == 0 && ctx_destroy(cb) == 0 && ctx_destroy(cc) == 0, "all three contexts destroyed (their channels are given back)");
   printf("nvgpu_hw_test rc: %d failure(s)\n", failures);
   return failures ? 1 : 0;
}

/// `nvgpu_hw_test clock [seconds [gap_ms [iterations]]]`: what clock the SMs run at, measured from inside the GPU (nvgpu/gen/shader/clock.cu: SM cycles over
/// the nanoseconds of the GPU's global timer across a chain of dependent FFMAs, one thread in each of 8 CTAs). Launches back to back for `seconds`
/// (`gap_ms` of sleep between them: 0 = a continuous load, more = a light duty cycle) and prints the clock of each launch (the first 8, then every
/// 250 ms) and what it did over time: GSP-RM changes the P-state by itself, so the clock at the first launch after an idle is what a client that
/// wakes the GPU gets. Nothing here fails on the value of the clock, only on a launch that did not run or numbers that cannot be a clock.
static int cmp_u64(const void *a, const void *b) {
   uint64_t x = *(const uint64_t *)a, y = *(const uint64_t *)b;
   return x < y ? -1 : x > y;
}

static int clock_run(int seconds, int gap_ms, uint32_t iters) {
   printf("nvgpu_hw_test clock: %d s, %d ms between launches, %u iterations per launch\n", seconds, gap_ms, iters);
   fd = open("/dev/nvgpu", O_RDWR);
   CHECK(fd >= 0, "open");
   if (fd < 0) return 1;
   struct nvg_info info;
   call(NVG_IOC_INFO, &info);
   hw = !(info.flags & NVG_INFO_SOFTWARE);
   if (!hw) {
      printf("  skip (software device: nothing runs)\n");
      return 0;
   }
   uint32_t ctx = ctx_create(NVG_ENGINE_COMPUTE), tl = sync_create(0);
   struct region r;
   REQUIRE(ctx && tl && region_make(&r, SLOT_BYTES) == 0, "a compute context, a timeline and a region");
   int64_t start = now_ms(), last_print = -1000;
   uint64_t seq = 0, bad = 0, lo = ~0ull, hi = 0, first = 0, ramp_ms = 0, smids = 0;
   // the ramp needs the maximum, known only at the end: keep every launch's median with its time
   static uint64_t med_log[1 << 17];
   static int64_t med_t[1 << 17];
   unsigned nmed = 0;
   while (now_ms() - start < (int64_t)seconds * 1000) {
      struct nvg_push p = launch_prepare_ex(&r, 0, nvg_shader_clock, sizeof nvg_shader_clock, 18, slot_out_va(&r, 0), iters, 0x600 + (uint32_t)seq);
      int64_t t_launch = now_us();
      seq++;
      if (exec_signal(ctx, &p, 1, tl, seq) != 0 || wait_timeline(tl, seq, 10000) != 0) {
         printf("  FAIL launch %llu did not complete [errno %d]\n", (unsigned long long)seq, errno);
         failures++;
         break;
      }
      int64_t took_us = now_us() - t_launch;
      const volatile uint64_t *o = (const volatile uint64_t *)slot_out(&r, 0);
      uint64_t mhz[8];
      unsigned nv = 0;
      uint64_t dur_ns = 0, cyc = 0;
      for (int c = 0; c < 8; c++) {
         uint64_t c0 = o[c * 8], c1 = o[c * 8 + 1], t0 = o[c * 8 + 2], t1 = o[c * 8 + 3];
         if (c1 > c0 && t1 > t0) {
            mhz[nv++] = (c1 - c0) * 1000 / (t1 - t0);
            if (!dur_ns) { dur_ns = t1 - t0; cyc = c1 - c0; }
            smids |= 1ull << (o[c * 8 + 4] & 63);
         }
      }
      if (nv == 0) {
         bad++;
         if (bad == 1) printf("  FAIL launch %llu wrote no sample: words %#llx %#llx %#llx %#llx\n", (unsigned long long)seq, (unsigned long long)o[0], (unsigned long long)o[1], (unsigned long long)o[2], (unsigned long long)o[3]);
         continue;
      }
      qsort(mhz, nv, sizeof mhz[0], cmp_u64);
      uint64_t med = mhz[nv / 2];
      int64_t t_ms = now_ms() - start;
      if (!first) first = med;
      if (med < lo) lo = med;
      if (med > hi) hi = med;
      if (nmed < (1u << 17)) { med_log[nmed] = med; med_t[nmed] = t_ms; nmed++; }
      if (seq <= 8 || t_ms - last_print >= 250) {
         printf("CLOCK t=%lld ms launch %llu: %llu MHz (CTAs %llu..%llu, %u of 8 read), %llu cycles in %llu us, launch to fence %lld us\n", (long long)t_ms, (unsigned long long)seq,
                (unsigned long long)med, (unsigned long long)mhz[0], (unsigned long long)mhz[nv - 1], nv, (unsigned long long)cyc, (unsigned long long)(dur_ns / 1000), (long long)took_us);
         last_print = t_ms;
      }
      if (gap_ms > 0) nap_us(gap_ms * 1000L);
   }
   for (unsigned i = 0; i < nmed; i++)
      if (med_log[i] * 100 >= hi * 95) { ramp_ms = med_t[i]; break; }
   printf("CLOCK SUMMARY: %llu launches, %llu without a sample; first launch %llu MHz, lowest %llu, highest %llu; first reached 95%% of the highest at %llu ms; %d SMs seen\n",
          (unsigned long long)seq, (unsigned long long)bad, (unsigned long long)first, (unsigned long long)(lo == ~0ull ? 0 : lo), (unsigned long long)hi, (unsigned long long)ramp_ms, __builtin_popcountll(smids));
   CHECK(seq > 0 && bad == 0, "every launch ran and wrote a sample (%llu launches, %llu without)", (unsigned long long)seq, (unsigned long long)bad);
   CHECK(hi >= 100 && hi <= 3500 && lo >= 50, "the numbers can be an SM clock (%llu..%llu MHz)", (unsigned long long)(lo == ~0ull ? 0 : lo), (unsigned long long)hi);
   ctx_destroy(ctx);
   printf("nvgpu_hw_test clock: %d failure(s)\n", failures);
   return failures ? 1 : 0;
}

int main(int argc, char **argv) {
   if (argc > 1 && !strcmp(argv[1], "clock")) return clock_run(argc > 2 ? atoi(argv[2]) : 10, argc > 3 ? atoi(argv[3]) : 0, argc > 4 ? (uint32_t)atoi(argv[4]) : 400000);
   if (argc > 1 && !strcmp(argv[1], "rc")) return rc_test();
   if (argc > 1 && !strcmp(argv[1], "grcopy")) return grcopy(0);
   if (argc > 1 && !strcmp(argv[1], "grcopy-bind")) return grcopy(1);
   printf("nvgpu_hw_test:\n");
   int bad = 0;

   // ---- the device
   fd = open("/dev/nvgpu", O_RDWR);
   CHECK(fd >= 0, "open /dev/nvgpu");
   if (fd < 0) return 1;
   struct nvg_info info;
   memset(&info, 0, sizeof info);
   CHECK(call(NVG_IOC_INFO, &info) == 0, "INFO");
   hw = !(info.flags & NVG_INFO_SOFTWARE);
   printf("  device: %s, %s, sm %u, %u GPCs, %u TPCs, %llu MiB of VRAM for buffers\n", info.device_name, hw ? "hardware" : "SOFTWARE (no GPU: execution checks are skipped)", info.sm,
          info.gpc_count, info.tpc_count, (unsigned long long)(info.vram_size_B >> 20));
   CHECK(info.abi_version == NVG_ABI_VERSION && info.sm == 86 && info.cls_compute == 0xc7c0, "abi %u sm %u compute %#x", info.abi_version, info.sm, info.cls_compute);
   CHECK(info.va_start < info.va_end && info.vram_size_B > 0 && info.bar_size_B == 0, "ranges");

   uint64_t t1 = 0, t2 = 0;
   struct nvg_timestamp ts;
   CHECK(call(NVG_IOC_TIMESTAMP, &ts) == 0, "TIMESTAMP");
   t1 = ts.ns;
   nap_us(2000);
   CHECK(call(NVG_IOC_TIMESTAMP, &ts) == 0, "TIMESTAMP again");
   t2 = ts.ns;
   CHECK(t2 > t1 && t2 - t1 < 5000000000ull, "the GPU timer moves: %llu -> %llu ns", (unsigned long long)t1, (unsigned long long)t2);
   if (hw) {
      CHECK(ctx_create(NVG_ENGINE_COPY | NVG_ENGINE_2D | NVG_ENGINE_3D | NVG_ENGINE_M2MF | NVG_ENGINE_COMPUTE) == 0 && errno == EINVAL,
            "a context asking for 2D or M2MF is EINVAL (no such objects)");
   }
   uint32_t ctx = ctx_create(NVG_ENGINE_COMPUTE);
   CHECK(ctx != 0, "a compute context");
   uint32_t tl = sync_create(0);
   CHECK(tl != 0, "a timeline");
   uint64_t seq = 0;   // the timeline value of the last submission

   // ---- 1. fill -> system memory, read by the CPU
   struct region host;
   CHECK(region_make(&host, 40 * SLOT_BYTES) == 0, "a system region of %d slots mapped and bound", 40);
   struct nvg_push p = launch_prepare(&host, 0, nvg_shader_fill, slot_out_va(&host, 0), 0, 0x4242);
   int64_t t0 = now_us();
   CHECK(exec_signal(ctx, &p, 1, tl, ++seq) == 0, "EXEC of the fill launch");
   CHECK(wait_timeline(tl, seq, 5000) == 0, "the fence of the fill launch comes");
   int64_t us = now_us() - t0;
   int first = 0;
   HWCHECK(out_is(&host, 0, fill_word, &first), "the 256 words are fill_word(i) and the other 768 are untouched (first bad word %d)", first);
   HWCHECK(slot_grid_sem(&host, 0) == 0x4242, "the grid's own release semaphore holds %#x", slot_grid_sem(&host, 0));
   printf("  fill -> host: EXEC to fence in %lld us\n", (long long)us);

   // ---- 2. fill -> VRAM, read back by the GPU in the same EXEC (two push segments)
   uint32_t vb = bo_create(4096, NVG_BO_VRAM, NULL);
   uint64_t vva = va_alloc(4096, 4096);
   CHECK(vb && vva && va_bind_call(vva, 4096, vb, 0) == 0, "a VRAM page bound");
   struct nvg_push two[2];
   two[0] = launch_prepare(&host, 1, nvg_shader_fill, vva, 0, 0x4243);
   two[1] = launch_prepare(&host, 2, nvg_shader_copy, slot_out_va(&host, 2), vva, 0x4244);
   CHECK(exec_signal(ctx, two, 2, tl, ++seq) == 0, "EXEC of fill -> VRAM then copy VRAM -> host (2 segments)");
   CHECK(wait_timeline(tl, seq, 5000) == 0, "its fence comes");
   HWCHECK(out_is(&host, 2, fill_word, &first), "the GPU read back what the GPU wrote to VRAM (first bad word %d)", first);
   HWCHECK(slot_grid_sem(&host, 1) == 0x4243 && slot_grid_sem(&host, 2) == 0x4244, "both grids released their semaphores");

   // ---- 2b. the copy engine alone (NVK's upload queue): host -> VRAM -> host, the pushes carry no SET_OBJECT
   {
      uint32_t cctx = ctx_create(NVG_ENGINE_COPY);
      CHECK(cctx != 0, "a copy-only context");
      CHECK(ctx_create(NVG_ENGINE_COMPUTE | NVG_ENGINE_COPY) != 0, "a compute + copy context");
      CHECK(ctx_create(NVG_ENGINE_3D | NVG_ENGINE_COMPUTE | NVG_ENGINE_COPY) != 0, "a 3D + compute + copy context (what NVK's queue families ask for)");
      uint32_t cb = bo_create(4096, NVG_BO_VRAM, NULL);
      uint64_t cva = va_alloc(4096, 4096);
      CHECK(cb && cva && va_bind_call(cva, 4096, cb, 0) == 0, "a VRAM page for the copy");
      uint32_t *csrc = (uint32_t *)(host.cpu + 37ull * SLOT_BYTES + OFF_OUT), *cdst = (uint32_t *)(host.cpu + 38ull * SLOT_BYTES + OFF_OUT);
      for (uint32_t i = 0; i < 1024; i++) { csrc[i] = fill_word(i) ^ 0x0badf00du; cdst[i] = scribble(i); }
      uint32_t w[32];
      uint32_t n = copy_push(w, host.va + 37ull * SLOT_BYTES + OFF_OUT, cva, 4096);
      n += copy_push(w + n, cva, host.va + 38ull * SLOT_BYTES + OFF_OUT, 4096);
      memcpy(host.cpu + 37ull * SLOT_BYTES + OFF_PUSH, w, n * 4);
      struct nvg_push cp = { .va = host.va + 37ull * SLOT_BYTES + OFF_PUSH, .bytes = n * 4, .flags = 0 };
      uint32_t ctl = sync_create(0);
      int64_t c0 = now_us();
      CHECK(ctl && exec_signal(cctx, &cp, 1, ctl, 1) == 0 && wait_timeline(ctl, 1, 5000) == 0, "EXEC of host -> VRAM -> host on the copy context");
      int same = 1;
      for (uint32_t i = 0; i < 1024; i++) same &= cdst[i] == csrc[i];
      HWCHECK(same, "the 4 KiB came back from VRAM intact");
      printf("  copy context: host -> VRAM -> host in %lld us\n", (long long)(now_us() - c0));
   }

   // ---- 3. rebinding: the VA must follow the new buffer (the GPU's TLB is flushed by bind and unbind)
   {
      uint32_t x = bo_create(4096, NVG_BO_VRAM, NULL), y = bo_create(4096, NVG_BO_VRAM, NULL);
      uint64_t vx = va_alloc(4096, 4096), vt = va_alloc(4096, 4096);
      CHECK(x && y && vx && vt && va_bind_call(vx, 4096, x, 0) == 0 && va_bind_call(vt, 4096, x, 0) == 0, "X bound at two VAs");
      // X gets fillwt's words through VX
      struct nvg_push a = launch_prepare(&host, 3, nvg_shader_fillwt, vx, 0, 1);
      // and is read through VT once, so the GPU has VT's translation cached
      struct nvg_push b = launch_prepare(&host, 4, nvg_shader_copy, slot_out_va(&host, 4), vt, 2);
      struct nvg_push ab[2] = { a, b };
      CHECK(exec_signal(ctx, ab, 2, tl, ++seq) == 0 && wait_timeline(tl, seq, 5000) == 0, "fillwt -> X (via VX), copy X (via VT)");
      HWCHECK(out_is(&host, 4, fillwt_word, &first), "VT reads X's fillwt words (first bad word %d)", first);
      // now VT moves to Y: unbind, bind Y, fill (the fill words) through VT
      CHECK(unbind(vt, 4096) == 0 && va_bind_call(vt, 4096, y, 0) == 0, "VT unbound and bound to Y");
      struct nvg_push c = launch_prepare(&host, 5, nvg_shader_fill, vt, 0, 3);
      struct nvg_push d = launch_prepare(&host, 6, nvg_shader_copy, slot_out_va(&host, 6), vt, 4);
      struct nvg_push e = launch_prepare(&host, 7, nvg_shader_copy, slot_out_va(&host, 7), vx, 5);
      struct nvg_push cde[3] = { c, d, e };
      CHECK(exec_signal(ctx, cde, 3, tl, ++seq) == 0 && wait_timeline(tl, seq, 5000) == 0, "fill -> Y (via VT), copy Y (via VT), copy X (via VX)");
      HWCHECK(out_is(&host, 6, fill_word, &first), "VT now reads Y's fill words (first bad word %d)", first);
      HWCHECK(out_is(&host, 7, fillwt_word, &first), "X still holds its fillwt words: the fill through VT did not reach the old buffer (first bad word %d)", first);
      CHECK(unbind(vt, 4096) == 0 && unbind(vx, 4096) == 0, "both unbound");
   }
   // the same with system memory, checked by the CPU: BO1, then BO2, behind one VA
   {
      struct region b1, b2;
      CHECK(region_make(&b1, SLOT_BYTES) == 0 && region_make(&b2, SLOT_BYTES) == 0, "two one-slot system regions");
      uint64_t vs = va_alloc(SLOT_BYTES, 0x10000);
      CHECK(vs != 0 && unbind(b1.va, b1.size) == 0 && unbind(b2.va, b2.size) == 0, "their own VAs unbound to make room for a shared one");
      // the launch's own pages travel with the region, so keep them at b1/b2's slots and put only the *output* behind the shared VA
      CHECK(va_bind_call(vs, SLOT_BYTES, b1.bo, 0) == 0, "VS bound to the first buffer");
      struct nvg_push a = launch_prepare(&host, 8, nvg_shader_fill, vs + OFF_OUT, 0, 6);
      memset(b1.cpu, 0, SLOT_BYTES);
      uint32_t *o1 = (uint32_t *)(b1.cpu + OFF_OUT);
      for (uint32_t i = 0; i < 1024; i++) o1[i] = scribble(i);
      CHECK(exec_signal(ctx, &a, 1, tl, ++seq) == 0 && wait_timeline(tl, seq, 5000) == 0, "fill through VS into the first buffer");
      int ok1 = 1;
      for (uint32_t i = 0; i < 256; i++) ok1 &= o1[i] == fill_word(i);
      HWCHECK(ok1, "the first buffer holds the fill words");
      for (uint32_t i = 0; i < 1024; i++) o1[i] = scribble(i);
      CHECK(unbind(vs, SLOT_BYTES) == 0 && va_bind_call(vs, SLOT_BYTES, b2.bo, 0) == 0, "VS moved to the second buffer");
      uint32_t *o2 = (uint32_t *)(b2.cpu + OFF_OUT);
      for (uint32_t i = 0; i < 1024; i++) o2[i] = scribble(i);
      struct nvg_push a2 = launch_prepare(&host, 9, nvg_shader_fill, vs + OFF_OUT, 0, 7);
      CHECK(exec_signal(ctx, &a2, 1, tl, ++seq) == 0 && wait_timeline(tl, seq, 5000) == 0, "fill through VS into the second buffer");
      int ok2 = 1, old_untouched = 1;
      for (uint32_t i = 0; i < 256; i++) ok2 &= o2[i] == fill_word(i);
      for (uint32_t i = 0; i < 1024; i++) old_untouched &= o1[i] == scribble(i);
      HWCHECK(ok2, "the second buffer holds the fill words");
      HWCHECK(old_untouched, "the first buffer, scribbled again after its launch, was not written by the second: no stale translation");
      unbind(vs, SLOT_BYTES);
      munmap(b1.cpu, b1.size); munmap(b2.cpu, b2.size);
   }

   // ---- 4. the ring: 700 submissions through a ring of 1024 entries and 64 fence slots (EAGAIN on the way), then 32 launches together
   {
      uint32_t noop[2] = { hdr(1, 0, 1), 0xc7c0 };   // SET_OBJECT: does nothing but bind the object
      memcpy(host.cpu + OFF_PUSH + 39 * SLOT_BYTES, noop, 8);
      struct nvg_push n = { .va = host.va + OFF_PUSH + 39 * SLOT_BYTES, .bytes = 8, .flags = 0 };
      int64_t s = now_us();
      int fails = 0;
      for (int i = 0; i < 700; i++) fails += exec_signal(ctx, &n, 1, tl, ++seq) != 0;
      CHECK(fails == 0, "700 back-to-back EXECs accepted (%d failed, errno %d)", fails, errno);
      CHECK(wait_timeline(tl, seq, 20000) == 0, "the last of them completes");
      uint64_t v = 0;
      CHECK(sync_value(tl, &v) == 0 && v == seq, "the timeline is at %llu, wanted %llu", (unsigned long long)v, (unsigned long long)seq);
      printf("  700 empty EXECs: %lld us in all\n", (long long)(now_us() - s));

      enum { N = 29 };   // slots 10..38 of the region
      struct nvg_push all[N];
      for (unsigned k = 0; k < N; k++) all[k] = launch_prepare(&host, 10 + k, nvg_shader_fill, slot_out_va(&host, 10 + k), 0, 100 + k);
      s = now_us();
      int fails2 = 0;
      for (unsigned k = 0; k < N; k++) fails2 += exec_signal(ctx, &all[k], 1, tl, ++seq) != 0;
      CHECK(fails2 == 0 && wait_timeline(tl, seq, 10000) == 0, "%d launches in flight together", N);
      int good = 0;
      for (unsigned k = 0; k < N; k++) good += out_is(&host, 10 + k, fill_word, &first);
      HWCHECK(good == N, "%d of %d output pages are right", good, N);
      printf("  %d launches in flight: %lld us in all\n", N, (long long)(now_us() - s));
   }

   // ---- 4b. a full ring on purpose: 300 copies of 32 MiB VRAM -> VRAM on the copy channel, submitted far faster than they run, so the kernel's
   // 64 fence slots fill and it answers EAGAIN (what the retry loop in exec_signal is for)
   {
      uint32_t cctx = ctx_create(NVG_ENGINE_COPY);
      uint64_t big = 32ull << 20;
      uint32_t ba = bo_create(big, NVG_BO_VRAM, NULL), bb = bo_create(big, NVG_BO_VRAM, NULL);
      uint64_t va_a = va_alloc(big, 0x10000), va_b = va_alloc(big, 0x10000);
      CHECK(cctx && ba && bb && va_a && va_b && va_bind_call(va_a, big, ba, 0) == 0 && va_bind_call(va_b, big, bb, 0) == 0, "two 32 MiB VRAM buffers");
      uint32_t w[16];
      uint32_t n = copy_push(w, va_a, va_b, (uint32_t)big);
      memcpy(host.cpu + 36ull * SLOT_BYTES + OFF_PUSH, w, n * 4);
      struct nvg_push cp = { .va = host.va + 36ull * SLOT_BYTES + OFF_PUSH, .bytes = n * 4, .flags = 0 };
      uint32_t t2 = sync_create(0);
      unsigned long before = eagains;
      int64_t s0 = now_us();
      int fails3 = 0;
      for (int i = 1; i <= 300; i++) fails3 += exec_signal(cctx, &cp, 1, t2, i) != 0;
      CHECK(fails3 == 0 && wait_timeline(t2, 300, 20000) == 0, "300 big copies all complete (%d rejected)", fails3);
      printf("  300 x 32 MiB VRAM copies: %lld us in all, %lu EAGAINs\n", (long long)(now_us() - s0), eagains - before);
      HWCHECK(eagains > before, "the ring/fence slots filled at least once (EAGAIN seen %lu times)", eagains - before);
      unbind(va_a, big); unbind(va_b, big);
      struct nvg_bo_free f1 = { .handle = ba }, f2 = { .handle = bb };
      call(NVG_IOC_BO_FREE, &f1); call(NVG_IOC_BO_FREE, &f2);
   }

   // ---- 4c. one channel per context: four compute contexts at once (the first takes the boot's channel, the others are made here), a launch
   // on each in flight together, every output checked; then the contexts are destroyed (channels given back) and made again (slots reused)
   {
      enum { K = 4 };
      for (int round = 0; round < 2; round++) {
         uint32_t cx[K], tx[K];
         int made = 1;
         for (int i = 0; i < K; i++) {
            cx[i] = ctx_create(NVG_ENGINE_3D | NVG_ENGINE_COMPUTE | NVG_ENGINE_COPY);
            tx[i] = sync_create(0);
            made &= cx[i] != 0 && tx[i] != 0;
         }
         REQUIRE(made, "round %d: %d 3D+compute+copy contexts (%u %u %u %u)", round, K, cx[0], cx[1], cx[2], cx[3]);
         struct nvg_push pp[K];
         for (int i = 0; i < K; i++) pp[i] = launch_prepare(&host, 5 + i, nvg_shader_fill, slot_out_va(&host, 5 + i), 0, 200 + i);
         int okx = 1;
         for (int i = 0; i < K; i++) okx &= exec_signal(cx[i], &pp[i], 1, tx[i], 1) == 0;
         for (int i = 0; i < K; i++) okx &= wait_timeline(tx[i], 1, 10000) == 0;
         CHECK(okx, "round %d: a launch on each context, all fences come", round);
         int good4 = 0;
         for (int i = 0; i < K; i++) good4 += out_is(&host, 5 + i, fill_word, &first);
         HWCHECK(good4 == K, "round %d: %d of %d outputs are right", round, good4, K);
         int gone = 1;
         for (int i = 0; i < K; i++) gone &= ctx_destroy(cx[i]) == 0;
         CHECK(gone, "round %d: the contexts are destroyed", round);
      }
   }

   // ---- 4d. the screen: SCANOUT_INFO and the refusals of PRESENT (the real thing, with a rendered buffer, is vk_draw's)
   {
      struct nvg_scanout_info si;
      memset(&si, 0, sizeof si);
      int sr = call(NVG_IOC_SCANOUT_INFO, &si);
      if (hw) {
         CHECK(sr == 0 && si.width >= 640 && si.height >= 480 && si.pitch_B >= si.width * 4 && si.size_B == (uint64_t)si.pitch_B * si.height && si.format == NVG_SCANOUT_XRGB8888,
               "the display is %ux%u, pitch %u, %llu bytes (%d)", si.width, si.height, si.pitch_B, (unsigned long long)si.size_B, sr);
      } else {
         CHECK(sr < 0 && errno == ENODEV, "no display behind the software device (ENODEV)");
      }
      uint32_t big = bo_create(si.size_B ? si.size_B : 4096, NVG_BO_VRAM, NULL), small = bo_create(4096, NVG_BO_VRAM, NULL), sys = bo_create(4096, NVG_BO_SYSTEM, NULL);
      struct nvg_present pr = { .handle = small, .offset = 0 };
      int err_small = call(NVG_IOC_PRESENT, &pr) < 0 ? errno : 0;
      pr = (struct nvg_present){ .handle = sys, .offset = 0 };
      int err_sys = call(NVG_IOC_PRESENT, &pr) < 0 ? errno : 0;
      pr = (struct nvg_present){ .handle = big, .offset = 8 };
      int err_align = call(NVG_IOC_PRESENT, &pr) < 0 ? errno : 0;
      pr = (struct nvg_present){ .handle = big, .flags = 1, .offset = 0 };
      int err_flags = call(NVG_IOC_PRESENT, &pr) < 0 ? errno : 0;
      pr = (struct nvg_present){ .handle = 0xdead, .offset = 0 };
      int err_none = call(NVG_IOC_PRESENT, &pr) < 0 ? errno : 0;
      int want = hw ? EINVAL : ENODEV;
      CHECK(big && small && sys, "buffers for the PRESENT refusals");
      CHECK(err_small == want && err_sys == want && err_align == want && err_flags == want && err_none == want,
            "PRESENT refuses a buffer smaller than the screen (%d), a system BO (%d), a misaligned offset (%d), flags (%d) and an unknown handle (%d): all %d",
            err_small, err_sys, err_align, err_flags, err_none, want);
   }

   // ---- 4e. PRESENT holds what it shows: freeing a buffer's handle while the display scans it out must not give its VRAM back (a WSI destroying a
   // swapchain does exactly that). The display reads the latest buffer and the one it replaced; the older one is let go at the next present. Section 5
   // then checks that closing the device releases the rest (a fresh session finds the VRAM heap empty).
   if (hw) {
      struct nvg_scanout_info si;
      struct nvg_info base_info;
      call(NVG_IOC_INFO, &base_info);
      if (call(NVG_IOC_SCANOUT_INFO, &si) == 0) {
         const uint64_t S = (si.size_B + 4095) & ~4095ull;
         uint32_t b[3];
         for (int i = 0; i < 3; i++) b[i] = bo_create(si.size_B, NVG_BO_VRAM, NULL);
         struct nvg_info all_info;
         call(NVG_IOC_INFO, &all_info);
         CHECK(b[0] && b[1] && b[2] && all_info.vram_used_B == base_info.vram_used_B + 3 * S,
               "three screen-sized buffers take %llu bytes of VRAM (%llu)", (unsigned long long)(3 * S), (unsigned long long)(all_info.vram_used_B - base_info.vram_used_B));
         int shown_ok = 1;
         uint64_t used_after_free_a = 0, used_b = 0, used_c = 0, used_freed = 0;
         for (int i = 0; i < 3; i++) {
            // the previous flip must have taken effect before the next present
            struct nvg_flip_state fs = { 0 };
            for (int t = 0; t < 500; t++) {
               if (call(NVG_IOC_FLIP_STATE, &fs) != 0 || !fs.pending) break;
               nap_us(2000);
            }
            struct nvg_present pr = { .handle = b[i], .offset = 0 };
            int pr_rc = -1;
            for (int t = 0; t < 200 && pr_rc != 0; t++) {
               pr_rc = call(NVG_IOC_PRESENT, &pr);
               if (pr_rc != 0) nap_us(1000);
            }
            if (pr_rc != 0) { shown_ok = 0; break; }
            struct nvg_info cur;
            if (i == 0) {
               struct nvg_bo_free f = { .handle = b[0] };
               call(NVG_IOC_BO_FREE, &f);
               call(NVG_IOC_INFO, &cur);
               used_after_free_a = cur.vram_used_B;
            } else {
               call(NVG_IOC_INFO, &cur);
               if (i == 1) used_b = cur.vram_used_B; else used_c = cur.vram_used_B;
            }
         }
         CHECK(shown_ok, "three buffers were put on the screen one after the other");
         CHECK(used_after_free_a == all_info.vram_used_B, "the freed buffer's VRAM is still held while the display may read it (%llu, was %llu)",
               (unsigned long long)used_after_free_a, (unsigned long long)all_info.vram_used_B);
         CHECK(used_b == all_info.vram_used_B, "still held after the second present (the display may still show it: %llu)", (unsigned long long)used_b);
         CHECK(used_c == all_info.vram_used_B - S, "the third present lets the first go: %llu, want %llu", (unsigned long long)used_c,
               (unsigned long long)(all_info.vram_used_B - S));
         for (int i = 1; i < 3; i++) { struct nvg_bo_free f = { .handle = b[i] }; call(NVG_IOC_BO_FREE, &f); }
         struct nvg_info fin;
         call(NVG_IOC_INFO, &fin);
         used_freed = fin.vram_used_B;
         printf("  present holds: buffer %llu bytes; VRAM in use base %llu, three buffers %llu, a freed while shown %llu, third shown %llu, last two freed %llu\n",
                (unsigned long long)S, (unsigned long long)base_info.vram_used_B, (unsigned long long)all_info.vram_used_B,
                (unsigned long long)used_after_free_a, (unsigned long long)used_c, (unsigned long long)used_freed);
         CHECK(used_freed == used_c, "freeing the last two handles returns nothing while they are on screen (%llu, was %llu)", (unsigned long long)used_freed,
               (unsigned long long)used_c);
      }
   }

   // ---- 5. the device is handed back: close, reopen, and a holder that dies with work in flight
   uint64_t before_vram = 0;
   {
      struct nvg_info i2;
      call(NVG_IOC_INFO, &i2);
      before_vram = i2.vram_used_B;
   }
   CHECK(before_vram >= 4 * 4096, "VRAM in use before closing: %llu (at least four pages: the read-back page, the copy page, X and Y; the screen test adds more)", (unsigned long long)before_vram);
   close(fd);
   fd = open("/dev/nvgpu", O_RDWR);
   CHECK(fd >= 0, "the device opens again after a close");
   if (fd < 0) return 1;
   {
      struct nvg_info i2;
      CHECK(call(NVG_IOC_INFO, &i2) == 0 && i2.vram_used_B == 0, "a fresh session holds no VRAM (%llu)", (unsigned long long)i2.vram_used_B);
   }
   pid_t child = fork();
   if (child == 0) {
      // the holder: launch and die without waiting
      struct region r;
      uint32_t c = ctx_create(NVG_ENGINE_COMPUTE), t = sync_create(0);
      if (!c || !t || region_make(&r, 2 * SLOT_BYTES) != 0) _exit(10);
      struct nvg_push q = launch_prepare(&r, 0, nvg_shader_fill, slot_out_va(&r, 0), 0, 9);
      if (exec_signal(c, &q, 1, t, 1) != 0) _exit(11);
      _exit(0);
   }
   close(fd);   // the child's copy of the description keeps the session alive until it dies
   int st = 0;
   waitpid(child, &st, 0);
   CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "the child that died with a launch in flight exited %d", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
   fd = open("/dev/nvgpu", O_RDWR);
   CHECK(fd >= 0, "the device opens after its holder died (errno %d)", errno);
   if (fd < 0) return 1;
   {
      struct region r;
      uint32_t c = ctx_create(NVG_ENGINE_COMPUTE), t = sync_create(0);
      CHECK(c && t && region_make(&r, SLOT_BYTES) == 0, "a fresh context, timeline and region");
      struct nvg_push q = launch_prepare(&r, 0, nvg_shader_fill, slot_out_va(&r, 0), 0, 10);
      CHECK(exec_signal(c, &q, 1, t, 1) == 0 && wait_timeline(t, 1, 5000) == 0, "a launch after the holder died runs");
      HWCHECK(out_is(&r, 0, fill_word, &first), "and its output is right (first bad word %d)", first);
   }

   // ---- 6. several clients at once (G5 layer 1): this process and three children, each with a session of its own, launch together.
   //         Every child opens its session and reports its first VA, then waits for the go byte: all four sessions are open at the same
   //         time before any work starts, and the launches overlap.
   {
      enum { KIDS = 3, ROUNDS = 40 };
      pid_t kid[KIDS];
      int rd[KIDS], go[KIDS];
      int forked = 1;
      for (int k = 0; k < KIDS; k++) {
         int pp[2], gp[2];
         if (pipe(pp) != 0 || pipe(gp) != 0) { forked = 0; break; }
         kid[k] = fork();
         if (kid[k] == 0) {
            close(pp[0]);
            close(gp[1]);
            close(fd);
            fd = open("/dev/nvgpu", O_RDWR);   // its own session: the parent's description is not this child's client
            struct nvg_info mine;
            uint64_t va0 = 0;
            if (fd < 0 || call(NVG_IOC_INFO, &mine) != 0) _exit(100);
            va0 = mine.va_start;
            char b;
            if (write(pp[1], &va0, sizeof va0) != sizeof va0 || read(gp[0], &b, 1) != 1) _exit(101);
            _exit(client(k + 1, ROUNDS, &va0));
         }
         close(pp[1]);
         close(gp[0]);
         rd[k] = pp[0];
         go[k] = gp[1];
      }
      CHECK(forked, "three children forked");
      uint64_t va_first[KIDS + 1] = { 0 };
      struct nvg_info me;
      CHECK(call(NVG_IOC_INFO, &me) == 0, "INFO of this process's session");
      va_first[0] = me.va_start;
      for (int k = 0; forked && k < KIDS; k++) {
         uint64_t v = 0;
         if (read(rd[k], &v, sizeof v) != sizeof v) v = 0;
         va_first[k + 1] = v;
      }
      for (int k = 0; forked && k < KIDS; k++) { char b = 1; if (write(go[k], &b, 1) != 1) forked = 0; close(go[k]); }
      uint64_t unused = 0;
      int rc0 = client(0, ROUNDS, &unused);
      CHECK(rc0 == 0, "this process's own client: step %d failed", rc0);
      int all_ok = 1;
      for (int k = 0; forked && k < KIDS; k++) {
         int stc = 0;
         waitpid(kid[k], &stc, 0);
         int code = WIFEXITED(stc) ? WEXITSTATUS(stc) : -1;
         CHECK(code == 0, "child %d's client: exit %d (the step that failed; 100 = open or INFO, 101 = pipe)", k + 1, code);
         all_ok &= code == 0;
         close(rd[k]);
      }
      int distinct = 1;
      for (int a = 0; a <= KIDS; a++)
         for (int b = a + 1; b <= KIDS; b++) distinct &= va_first[a] != va_first[b] && va_first[a] && va_first[b];
      CHECK(distinct, "the four sessions have four different VA ranges (%#llx %#llx %#llx %#llx)", (unsigned long long)va_first[0],
            (unsigned long long)va_first[1], (unsigned long long)va_first[2], (unsigned long long)va_first[3]);
      CHECK(all_ok, "%d launches each, all right, in four sessions at once", ROUNDS);
      struct nvg_info i4;
      CHECK(call(NVG_IOC_INFO, &i4) == 0, "INFO after the children are gone");
   }

   // ---- 7. a buffer shared between two processes (G5 layer 2). The parent's GPU fills a page of a shared system region and a VRAM page; a child,
   //         in a session of its own, imports both through descriptors it receives by SCM_RIGHTS, reads the region with its CPU (the parent's GPU
   //         wrote it), copies the VRAM page into the region with its own GPU (through a VA of its own), and writes its own result into the
   //         region with a second launch; the parent then reads what the child's GPU put in its pages.
   {
      struct region sh;
      uint32_t c7 = ctx_create(NVG_ENGINE_3D | NVG_ENGINE_COMPUTE | NVG_ENGINE_COPY), t7 = sync_create(0);
      uint32_t vbs = bo_create(4096, NVG_BO_VRAM, NULL);
      uint64_t vvs = va_alloc(4096, 4096);
      REQUIRE(region_make(&sh, 3 * SLOT_BYTES) == 0 && c7 && t7 && vbs && vvs && va_bind_call(vvs, 4096, vbs, 0) == 0, "a shared region, a VRAM page and a context");
      struct nvg_push q7[2];
      q7[0] = launch_prepare(&sh, 2, nvg_shader_fill, slot_out_va(&sh, 2), 0, 0x71);   // fill_word into the region's slot 2 output
      q7[1] = launch_prepare(&sh, 1, nvg_shader_fill, vvs, 0, 0x72);                   // fill_word into the VRAM page
      CHECK(exec_signal(c7, q7, 2, t7, 1) == 0 && wait_timeline(t7, 1, 10000) == 0, "the parent's two launches (region, VRAM page)");
      int e_region = bo_export(sh.bo), e_vram = bo_export(vbs);
      CHECK(e_region >= 0 && e_vram >= 0, "both exported (%d %d)", e_region, e_vram);
      int sv[2];
      REQUIRE(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "a socketpair");
      pid_t kid = fork();
      if (kid == 0) {
         close(sv[0]);
         close(fd);   // the parent's session (inherited): this process works in one of its own
         int r1 = recv_fd(sv[1]), r2 = recv_fd(sv[1]);
         fd = open("/dev/nvgpu", O_RDWR);
         if (r1 < 0 || r2 < 0 || fd < 0) _exit(10);
         struct region a, b;
         if (region_import(&a, r1) != 0 || region_import(&b, r2) != 0) _exit(11);
         int bad = 0;
         if (hw && !out_is(&a, 2, fill_word, &bad)) _exit(12);   // what the parent's GPU wrote, read by this CPU
         uint32_t cx = ctx_create(NVG_ENGINE_3D | NVG_ENGINE_COMPUTE | NVG_ENGINE_COPY), tx = sync_create(0);
         if (!cx || !tx) _exit(13);
         struct nvg_push qc[2];
         qc[0] = launch_prepare(&a, 0, nvg_shader_fillwt, slot_out_va(&a, 0), 0, 0x73);      // this GPU writes into the shared region
         qc[1] = launch_prepare(&a, 1, nvg_shader_copy, slot_out_va(&a, 1), b.va, 0x74);     // this GPU copies the shared VRAM page into it
         if (exec_signal(cx, qc, 2, tx, 1) != 0 || wait_timeline(tx, 1, 10000) != 0) _exit(14);
         if (hw && !out_is(&a, 0, fillwt_word, &bad)) _exit(15);
         if (hw && !out_is(&a, 1, fill_word, &bad)) _exit(16);   // the parent's GPU wrote this VRAM, this GPU read it through its own VA
         _exit(0);
      }
      close(sv[1]);
      CHECK(send_fd(sv[0], e_region) == 0 && send_fd(sv[0], e_vram) == 0, "both descriptors sent");
      close(e_region);
      close(e_vram);
      int st7 = 0;
      waitpid(kid, &st7, 0);
      int code7 = WIFEXITED(st7) ? WEXITSTATUS(st7) : -1;
      CHECK(code7 == 0, "the child imported both and ran its launches: exit %d (10 = setup, 11 = import, 12 = saw the parent's GPU output, 13 = context, 14 = exec, 15/16 = its own results)", code7);
      HWCHECK(out_is(&sh, 0, fillwt_word, &first), "the child's GPU wrote into the parent's pages (first bad word %d)", first);
      HWCHECK(out_is(&sh, 1, fill_word, &first), "and its copy of the parent's VRAM page is there too (first bad word %d)", first);
      HWCHECK(out_is(&sh, 2, fill_word, &first), "the parent's own output is intact (first bad word %d)", first);
      close(sv[0]);
      // the exporter's memory outlives the exporter: nothing is held once both processes are done with it
      ctx_destroy(c7);
   }

   // ---- 8. a timeline shared between two processes (G5 layer 2, explicit sync). The parent queues a launch that fills a shared region and
   //         signals a shared timeline to 1, sends both descriptors and then only waits on a socket: no GPU call, so nothing of the parent's
   //         advances the timeline. The child imports both, waits for the timeline and reads the region (what the parent's GPU wrote must be
   //         there once the timeline says 1), launches its own work signalling 2 and sits idle on the socket in turn; the parent, waiting on its
   //         original handle, must find 2 and the child's result in its pages. Neither side is "calling in" for the other.
   {
      struct region sh8;
      uint32_t c8 = ctx_create(NVG_ENGINE_3D | NVG_ENGINE_COMPUTE | NVG_ENGINE_COPY), t8 = sync_create(0);
      REQUIRE(c8 && t8 && region_make(&sh8, 2 * SLOT_BYTES) == 0, "a context, a timeline and a shared region");
      struct nvg_push q8 = launch_prepare(&sh8, 0, nvg_shader_fill, slot_out_va(&sh8, 0), 0, 0x81);
      int e_bo = bo_export(sh8.bo);
      int sv[2];
      REQUIRE(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "a socketpair");
      pid_t kid = fork();
      if (kid == 0) {
         close(sv[0]);
         close(fd);
         int r1 = recv_fd(sv[1]), r2 = recv_fd(sv[1]);
         fd = open("/dev/nvgpu", O_RDWR);
         if (r1 < 0 || r2 < 0 || fd < 0) _exit(10);
         struct region a;
         if (region_import(&a, r1) != 0) _exit(11);
         uint32_t h = sync_import(r2);
         if (!h) _exit(12);
         int bad = 0;
         if (wait_timeline(h, 1, 10000) != 0) _exit(13);                 // the parent is idle: only a reader can see its fence complete
         if (hw && !out_is(&a, 0, fill_word, &bad)) _exit(14);           // and when it says 1, the parent's output is there
         uint32_t cx = ctx_create(NVG_ENGINE_3D | NVG_ENGINE_COMPUTE | NVG_ENGINE_COPY);
         if (!cx) _exit(15);
         struct nvg_push qc = launch_prepare(&a, 1, nvg_shader_fillwt, slot_out_va(&a, 1), 0, 0x82);
         if (exec_signal(cx, &qc, 1, h, 2) != 0) _exit(16);              // queued, not waited for
         char b = 'D';
         if (write(sv[1], &b, 1) != 1) _exit(17);
         if (read(sv[1], &b, 1) != 1) _exit(18);                         // idle until the parent has seen the result
         _exit(0);
      }
      close(sv[1]);
      CHECK(exec_signal(c8, &q8, 1, t8, 1) == 0, "the parent's launch, signalling the timeline, queued and not waited for");
      // exported AFTER the work was queued (the order a Vulkan program has: submit, then vkGetSemaphoreFdKHR): the signal queued before the
      // export must still be resolved for the child while the parent is idle
      int e_sync = sync_export(t8);
      CHECK(e_bo >= 0 && e_sync >= 0, "the region and the timeline exported (%d %d)", e_bo, e_sync);
      CHECK(send_fd(sv[0], e_bo) == 0 && send_fd(sv[0], e_sync) == 0, "both descriptors sent");
      close(e_bo);
      close(e_sync);
      char got = 0;
      CHECK(read(sv[0], &got, 1) == 1 && got == 'D', "the child has queued its own launch (it saw the parent's complete)");
      CHECK(wait_timeline(t8, 2, 10000) == 0, "the parent sees the child's launch complete on the shared timeline while the child is idle");
      HWCHECK(out_is(&sh8, 1, fillwt_word, &first), "and the child's output is in the parent's pages (first bad word %d)", first);
      HWCHECK(out_is(&sh8, 0, fill_word, &first), "the parent's own output is intact (first bad word %d)", first);
      char k = 'k';
      CHECK(write(sv[0], &k, 1) == 1, "the child is released");
      int st8 = 0;
      waitpid(kid, &st8, 0);
      int code8 = WIFEXITED(st8) ? WEXITSTATUS(st8) : -1;
      CHECK(code8 == 0, "the child's run: exit %d (10 setup, 11 region, 12 timeline, 13 never saw 1, 14 the parent's output was not there at 1, 15 context, 16 exec, 17-18 socket)", code8);
      close(sv[0]);
      ctx_destroy(c8);
   }

   bad = failures;
   printf("nvgpu_hw_test: %d failure(s)\n", bad);
   return bad ? 1 : 0;
}
