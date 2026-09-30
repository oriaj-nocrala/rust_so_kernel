// /dev/nvgpu on the hardware (G4c): the same interface nvgpu_sw_test.c checks against the software device, but with a GPU behind it. Real
// compute launches through EXEC, every result read back by a path other than the writer's:
//   - fill (a shader that stores 256 distinct words) into system memory, read by the CPU; the other 768 words of the page must stay the
//     scribble they started with;
//   - fill into VRAM, read back by the GPU (a second shader copies the page into system memory) in the same EXEC: two push segments;
//   - rebinding: a VA moved from one buffer to another (system and VRAM) must reach the new buffer, never the old one (the TLB flush);
//   - the ring: 700 submissions (the ring has 1024 entries, the kernel keeps 64 fences in flight) and 32 real launches in flight together;
//   - the device handed back: close and reopen, and a holder that dies with work in flight.
// On the software device (QEMU: no GPU) the execution checks are skipped and only the interface is exercised.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include "nvgpu.h"
#include "nvgpu_qmd.h"
#include "nvgpu_shaders.h"

extern int ioctl(int fd, unsigned long request, ...);

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf(" [errno %d]\n", errno); } } while (0)
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

static int bind(uint64_t va, uint64_t size, uint32_t handle, uint64_t bo_off) {
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

/// EXEC signalling `sync` = `value` when its pushes and the kernel's fence have run; retried while the ring is full (EAGAIN).
static int exec_signal(uint32_t ctx, const struct nvg_push *p, uint32_t np, uint32_t sync, uint64_t value) {
   struct nvg_sync_ref sig = { .handle = sync, .value = value };
   struct nvg_exec e = { .ctx = ctx, .push_count = np, .sig_count = 1, .pushes = (uintptr_t)p, .signals = (uintptr_t)&sig };
   int64_t deadline = now_ms() + 20000;
   for (;;) {
      int r = call(NVG_IOC_EXEC, &e);
      if (r == 0 || errno != EAGAIN) return r;
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
   return bind(r->va, size, r->bo, 0);
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

/// Set up slot `slot` of `r` to run `shader` (384 bytes, 8 registers) as 8 CTAs of 32 threads with kernel parameters `p0` and `p1`, and
/// scribble its output page. Returns the push segment to EXEC.
static struct nvg_push launch_prepare(struct region *r, unsigned slot, const uint8_t *shader, uint64_t p0, uint64_t p1, uint32_t release_payload) {
   uint8_t *cpu = r->cpu + (uint64_t)slot * SLOT_BYTES;
   uint64_t va = r->va + (uint64_t)slot * SLOT_BYTES;
   memcpy(cpu + OFF_SHADER, shader, 384);
   memset(cpu + OFF_CB0, 0, CBUF0_BYTES);
   memcpy(cpu + OFF_CB0 + PARAM0, &p0, 8);
   memcpy(cpu + OFF_CB0 + PARAM1, &p1, 8);
   struct nvg_qmd_launch l = {
      .program = va + OFF_SHADER, .registers = 8, .grid = { 8, 1, 1 }, .block = { 32, 1, 1 },
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

int main(void) {
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
   printf("  device: %s, %s, sm %u, %llu MiB of VRAM for buffers\n", info.device_name, hw ? "hardware" : "SOFTWARE (no GPU: execution checks are skipped)", info.sm,
          (unsigned long long)(info.vram_size_B >> 20));
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
   CHECK(vb && vva && bind(vva, 4096, vb, 0) == 0, "a VRAM page bound");
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
      CHECK(cb && cva && bind(cva, 4096, cb, 0) == 0, "a VRAM page for the copy");
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
      CHECK(x && y && vx && vt && bind(vx, 4096, x, 0) == 0 && bind(vt, 4096, x, 0) == 0, "X bound at two VAs");
      // X gets fillwt's words through VX
      struct nvg_push a = launch_prepare(&host, 3, nvg_shader_fillwt, vx, 0, 1);
      // and is read through VT once, so the GPU has VT's translation cached
      struct nvg_push b = launch_prepare(&host, 4, nvg_shader_copy, slot_out_va(&host, 4), vt, 2);
      struct nvg_push ab[2] = { a, b };
      CHECK(exec_signal(ctx, ab, 2, tl, ++seq) == 0 && wait_timeline(tl, seq, 5000) == 0, "fillwt -> X (via VX), copy X (via VT)");
      HWCHECK(out_is(&host, 4, fillwt_word, &first), "VT reads X's fillwt words (first bad word %d)", first);
      // now VT moves to Y: unbind, bind Y, fill (the fill words) through VT
      CHECK(unbind(vt, 4096) == 0 && bind(vt, 4096, y, 0) == 0, "VT unbound and bound to Y");
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
      CHECK(bind(vs, SLOT_BYTES, b1.bo, 0) == 0, "VS bound to the first buffer");
      struct nvg_push a = launch_prepare(&host, 8, nvg_shader_fill, vs + OFF_OUT, 0, 6);
      memset(b1.cpu, 0, SLOT_BYTES);
      uint32_t *o1 = (uint32_t *)(b1.cpu + OFF_OUT);
      for (uint32_t i = 0; i < 1024; i++) o1[i] = scribble(i);
      CHECK(exec_signal(ctx, &a, 1, tl, ++seq) == 0 && wait_timeline(tl, seq, 5000) == 0, "fill through VS into the first buffer");
      int ok1 = 1;
      for (uint32_t i = 0; i < 256; i++) ok1 &= o1[i] == fill_word(i);
      HWCHECK(ok1, "the first buffer holds the fill words");
      for (uint32_t i = 0; i < 1024; i++) o1[i] = scribble(i);
      CHECK(unbind(vs, SLOT_BYTES) == 0 && bind(vs, SLOT_BYTES, b2.bo, 0) == 0, "VS moved to the second buffer");
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

   // ---- 5. the device is handed back: close, reopen, and a holder that dies with work in flight
   uint64_t before_vram = 0;
   {
      struct nvg_info i2;
      call(NVG_IOC_INFO, &i2);
      before_vram = i2.vram_used_B;
   }
   CHECK(before_vram == 4 * 4096, "VRAM in use before closing: %llu (four pages: the read-back page, the copy page, X and Y)", (unsigned long long)before_vram);
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

   bad = failures;
   printf("nvgpu_hw_test: %d failure(s)\n", bad);
   return bad ? 1 : 0;
}
