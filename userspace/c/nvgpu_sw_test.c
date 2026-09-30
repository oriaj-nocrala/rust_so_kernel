// /dev/nvgpu, the software device (G4a): the whole interface of nvgpu/uapi/nvgpu.h, checked from user space. Bookkeeping and memory are
// real; execution is not, so this test proves what Mesa's backend will rely on: exclusive open, BOs mapped through the arena, VA
// allocation and binding with its error cases, contexts, EXEC with timeline waits and signals, and the device being handed back
// when its holder dies.
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include "nvgpu.h"

extern int ioctl(int fd, unsigned long request, ...);

static int failures;
#define CHECK(cond, ...) do { if (cond) printf("  ok   %s\n", #cond); else { failures++; printf("  FAIL %s (line %d): ", #cond, __LINE__); printf(__VA_ARGS__); printf(" [errno %d]\n", errno); } } while (0)

static int fd = -1;

static int mapped(const void *p) { return p != MAP_FAILED; }

static int call(unsigned long req, void *arg) { errno = 0; return ioctl(fd, req, arg); }

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

static int sync_query(uint32_t h, uint64_t *v) {
   struct nvg_sync_query q = { .handle = h };
   int r = call(NVG_IOC_SYNC_QUERY, &q);
   *v = q.value;
   return r;
}

static int sync_wait(const struct nvg_sync_ref *refs, uint32_t n, uint32_t flags, uint32_t *first) {
   struct nvg_sync_wait w = { .refs = (uintptr_t)refs, .count = n, .flags = flags };
   int r = call(NVG_IOC_SYNC_WAIT, &w);
   if (first) *first = w.first_ready;
   return r;
}

static int exec(uint32_t ctx, const struct nvg_push *p, uint32_t np, const struct nvg_sync_ref *w, uint32_t nw,
                const struct nvg_sync_ref *s, uint32_t ns) {
   struct nvg_exec e = { .ctx = ctx, .push_count = np, .wait_count = nw, .sig_count = ns,
                         .pushes = (uintptr_t)p, .waits = (uintptr_t)w, .signals = (uintptr_t)s };
   return call(NVG_IOC_EXEC, &e);
}

int main(void) {
   printf("nvgpu_sw_test:\n");

   // ---- open, exclusivity
   fd = open("/dev/nvgpu", O_RDWR);
   CHECK(fd >= 0, "open /dev/nvgpu");
   if (fd < 0) return 1;
   CHECK(open("/dev/nvgpu", O_RDWR) < 0 && errno == EBUSY, "a second open is EBUSY");

   struct nvg_info info;
   memset(&info, 0xff, sizeof info);
   CHECK(call(NVG_IOC_INFO, &info) == 0, "INFO");
   CHECK(info.abi_version == NVG_ABI_VERSION && (info.flags & NVG_INFO_SOFTWARE), "abi %u flags %#x", info.abi_version, info.flags);
   CHECK(info.sm == 86 && info.cls_compute == 0xc7c0 && info.bar_size_B == 0, "sm %u compute %#x bar %llu", info.sm, info.cls_compute,
         (unsigned long long)info.bar_size_B);
   CHECK(info.va_start < info.va_end && info.vram_size_B > 0 && strlen(info.device_name) > 0, "ranges and name");
   CHECK(call(0xc0004e77, &info) < 0 && errno == ENOTTY, "an unknown request is ENOTTY");

   // ---- BOs and the arena
   uint64_t off1 = 0, off2 = 0;
   uint32_t b1 = bo_create(8192, NVG_BO_SYSTEM, &off1);
   uint32_t b2 = bo_create(100, NVG_BO_SYSTEM, &off2);
   CHECK(b1 && b2 && b1 != b2, "two system BOs (%u %u)", b1, b2);
   CHECK(off1 % 4096 == 0 && off2 % 4096 == 0 && off1 != off2, "offsets %#llx %#llx", (unsigned long long)off1, (unsigned long long)off2);
   unsigned char *m1 = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, off1);
   unsigned char *m2 = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, off2);
   CHECK(mapped(m1) && mapped(m2), "mmap of both BOs");
   if (!mapped(m1) || !mapped(m2)) return 1;
   CHECK(m1[0] == 0 && m1[8191] == 0 && m2[0] == 0, "fresh BOs read as zeros");
   memset(m1, 0xa5, 8192);
   memset(m2, 0x5a, 4096);
   CHECK(m1[8191] == 0xa5 && m2[0] == 0x5a, "BOs are separate memory");
   unsigned char *again = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, off1);
   CHECK(mapped(again) && again[0] == 0xa5 && again[8191] == 0xa5, "the same BO mapped twice is the same memory");
   again[100] = 0x11;
   CHECK(m1[100] == 0x11, "a store through one mapping shows through the other");

   uint64_t voff = 0;
   uint32_t v1 = bo_create(1 << 20, NVG_BO_VRAM, &voff);
   CHECK(v1 != 0 && voff == ~0ull, "a VRAM BO has no mmap offset");
   struct nvg_info info2;
   call(NVG_IOC_INFO, &info2);
   CHECK(info2.vram_used_B == (1 << 20), "VRAM in use %llu", (unsigned long long)info2.vram_used_B);

   struct nvg_bo_create bad = { .size = 0, .flags = NVG_BO_SYSTEM };
   CHECK(call(NVG_IOC_BO_CREATE, &bad) < 0 && errno == EINVAL, "size 0 is EINVAL");
   bad = (struct nvg_bo_create){ .size = 4096, .flags = 9 };
   CHECK(call(NVG_IOC_BO_CREATE, &bad) < 0 && errno == EINVAL, "an unknown flag is EINVAL");
   bad = (struct nvg_bo_create){ .size = 1ull << 40, .flags = NVG_BO_SYSTEM };
   CHECK(call(NVG_IOC_BO_CREATE, &bad) < 0 && errno == ENOMEM, "more than the arena is ENOMEM");

   // ---- freeing a BO gives fresh zero pages to the next one at that offset
   munmap(again, 8192);
   munmap(m1, 8192);
   struct nvg_bo_free fr = { .handle = b1 };
   CHECK(call(NVG_IOC_BO_FREE, &fr) == 0, "BO_FREE");
   CHECK(call(NVG_IOC_BO_FREE, &fr) < 0 && errno == ENOENT, "freeing twice is ENOENT");
   uint64_t off3 = 0;
   uint32_t b3 = bo_create(8192, NVG_BO_SYSTEM, &off3);
   CHECK(b3 != 0 && off3 == off1, "the freed arena range is reused (%#llx vs %#llx)", (unsigned long long)off3, (unsigned long long)off1);
   unsigned char *m3 = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, off3);
   CHECK(mapped(m3) && m3[0] == 0 && m3[100] == 0 && m3[8191] == 0, "the reused range is zeroed, not the old contents");

   // ---- GPU virtual addresses
   uint64_t va = va_alloc(0x10000, 0x10000);
   CHECK(va != 0 && va % 0x10000 == 0 && va >= info.va_start && va + 0x10000 <= info.va_end, "VA alloc %#llx", (unsigned long long)va);
   CHECK(va_alloc(4096, 100) == 0 && errno == EINVAL, "align below a page is EINVAL");
   struct nvg_va_alloc withflags = { .size = 4096, .align = 4096, .flags = 1 };
   CHECK(call(NVG_IOC_VA_ALLOC, &withflags) < 0 && errno == EINVAL, "unknown flags are EINVAL");

   CHECK(bind(va, 8192, b3, 0) == 0, "bind 2 pages");
   CHECK(bind(va + 4096, 4096, b3, 0) < 0 && errno == EEXIST, "an overlapping bind is EEXIST");
   CHECK(bind(va + 1, 4096, b3, 0) < 0 && errno == EINVAL, "an unaligned bind is EINVAL");
   CHECK(bind(va + 0x8000, 12288, b3, 0) < 0 && errno == EINVAL, "past the end of the BO is EINVAL");
   CHECK(bind(va + 0x8000, 4096, 9999, 0) < 0 && errno == ENOENT, "an unknown BO is ENOENT");
   CHECK(bind(info.va_start - 4096, 4096, b3, 0) < 0 && errno == EINVAL, "outside any allocation is EINVAL");
   struct nvg_va_free vf = { .va = va, .size = 0x10000 };
   CHECK(call(NVG_IOC_VA_FREE, &vf) < 0 && errno == EBUSY, "freeing a range with bindings is EBUSY");
   CHECK(unbind(va + 4096, 4096) == 0, "unbind the second page (a cut)");
   CHECK(bind(va + 4096, 4096, b3, 4096) == 0, "and bind it again");
   CHECK(unbind(va, 0x10000) == 0, "unbind everything (gaps are fine)");
   CHECK(call(NVG_IOC_VA_FREE, &vf) == 0, "VA_FREE once nothing is bound");
   CHECK(call(NVG_IOC_VA_FREE, &vf) < 0 && errno == ENOENT, "VA_FREE twice is ENOENT");

   // ---- a BO closed while bound stays usable by the GPU until unbound
   uint64_t va2 = va_alloc(0x4000, 4096);
   struct nvg_bo_free fr3 = { .handle = b3 };
   CHECK(bind(va2, 8192, b3, 0) == 0 && call(NVG_IOC_BO_FREE, &fr3) == 0, "close a bound BO");
   CHECK(bind(va2 + 8192, 4096, b3, 0) < 0 && errno == ENOENT, "its handle is dead");
   CHECK(unbind(va2, 0x4000) == 0, "unbind releases it");

   // ---- contexts, EXEC and timelines
   struct nvg_ctx_create cc = { .engines = 0 };
   CHECK(call(NVG_IOC_CTX_CREATE, &cc) < 0 && errno == EINVAL, "no engines is EINVAL");
   cc.engines = NVG_ENGINE_COMPUTE | NVG_ENGINE_COPY;
   CHECK(call(NVG_IOC_CTX_CREATE, &cc) == 0 && cc.ctx != 0, "CTX_CREATE");

   uint64_t offp = 0;
   uint32_t bp = bo_create(8192, NVG_BO_SYSTEM, &offp);
   uint64_t vp = va_alloc(0x4000, 4096);
   CHECK(bind(vp, 8192, bp, 0) == 0, "bind the push memory");
   struct nvg_push ok[2] = { { .va = vp, .bytes = 64 }, { .va = vp + 4096 - 32, .bytes = 64 } };
   struct nvg_push hole = { .va = vp + 8192 - 16, .bytes = 32 };
   struct nvg_push odd = { .va = vp, .bytes = 6 };
   CHECK(exec(cc.ctx, ok, 2, NULL, 0, NULL, 0) == 0, "EXEC of two pushes (the second spans two pages)");
   CHECK(exec(cc.ctx, &hole, 1, NULL, 0, NULL, 0) < 0 && errno == EFAULT, "a push running past bound memory is EFAULT");
   CHECK(exec(cc.ctx, &odd, 1, NULL, 0, NULL, 0) < 0 && errno == EINVAL, "a push of 6 bytes is EINVAL");
   CHECK(exec(cc.ctx + 100, ok, 1, NULL, 0, NULL, 0) < 0 && errno == ENOENT, "an unknown context is ENOENT");

   uint32_t s = sync_create(0), gate = sync_create(0);
   CHECK(s && gate && s != gate, "two timelines");
   struct nvg_sync_ref sig = { .handle = s, .value = 5 };
   struct nvg_sync_ref wait_gate = { .handle = gate, .value = 1 };
   uint64_t v = 99;
   CHECK(exec(cc.ctx, ok, 1, &wait_gate, 1, &sig, 1) < 0 && errno == EAGAIN, "an EXEC whose wait is not ready is EAGAIN");
   CHECK(sync_query(s, &v) == 0 && v == 0, "and it signalled nothing (value %llu)", (unsigned long long)v);
   struct nvg_sync_signal ss = { .handle = gate, .value = 1 };
   CHECK(call(NVG_IOC_SYNC_SIGNAL, &ss) == 0, "signal the gate from the CPU");
   CHECK(exec(cc.ctx, ok, 1, &wait_gate, 1, &sig, 1) == 0, "now the EXEC goes through");
   CHECK(sync_query(s, &v) == 0 && v == 5, "and signals its timeline (value %llu)", (unsigned long long)v);
   ss.value = 0;
   CHECK(call(NVG_IOC_SYNC_SIGNAL, &ss) < 0 && errno == EINVAL, "a timeline cannot go backwards");

   struct nvg_sync_ref both[2] = { { .handle = s, .value = 5 }, { .handle = gate, .value = 2 } };
   uint32_t first = 77;
   CHECK(sync_wait(both, 2, 0, &first) < 0 && errno == EAGAIN, "waiting for all: EAGAIN while one is short");
   CHECK(sync_wait(both, 2, NVG_WAIT_ANY, &first) == 0 && first == 0, "waiting for any: ready at index %u", first);
   both[1].value = 1;
   CHECK(sync_wait(both, 2, 0, &first) == 0, "waiting for all: ready");
   struct nvg_sync_wait empty = { .refs = 0, .count = 0, .flags = 0 };
   CHECK(call(NVG_IOC_SYNC_WAIT, &empty) < 0 && errno == EINVAL, "a wait for nothing is EINVAL");
   struct nvg_sync_destroy sd = { .handle = gate };
   CHECK(call(NVG_IOC_SYNC_DESTROY, &sd) == 0 && sync_query(gate, &v) < 0 && errno == ENOENT, "a destroyed timeline is gone");

   struct nvg_timestamp t1, t2;
   CHECK(call(NVG_IOC_TIMESTAMP, &t1) == 0, "TIMESTAMP");
   struct timespec ts = { 0, 2000000 };
   nanosleep(&ts, NULL);
   call(NVG_IOC_TIMESTAMP, &t2);
   CHECK(t2.ns > t1.ns, "the clock advances (%llu -> %llu)", (unsigned long long)t1.ns, (unsigned long long)t2.ns);

   struct nvg_ctx_destroy cd = { .ctx = cc.ctx };
   CHECK(call(NVG_IOC_CTX_DESTROY, &cd) == 0 && call(NVG_IOC_CTX_DESTROY, &cd) < 0 && errno == ENOENT, "CTX_DESTROY once");

   // ---- the device is handed back when its last holder goes: by the time waitpid returns, the dead process's files are closed
   //      (Linux closes them in do_exit). Repeated, because the window it used to lose in was a few microseconds wide.
   int late = 0;
   for (int round = 0; round < 40; round++) {
      pid_t child = fork();
      if (child == 0) {
         pause();
         _exit(0);
      }
      close(fd);
      fd = open("/dev/nvgpu", O_RDWR);
      if (round == 0) CHECK(fd < 0 && errno == EBUSY, "the forked child still holds the session (open is %d, errno %d)", fd, errno);
      kill(child, SIGKILL);
      int status = 0;
      waitpid(child, &status, 0);
      fd = open("/dev/nvgpu", O_RDWR);
      if (fd < 0) {
         late++;
         for (int tries = 0; fd < 0 && tries < 200; tries++) {
            struct timespec nap = { 0, 1000000 };
            nanosleep(&nap, NULL);
            fd = open("/dev/nvgpu", O_RDWR);
         }
      }
      if (fd < 0) break;
   }
   CHECK(late == 0, "the device was still busy right after waitpid in %d of 40 rounds", late);
   CHECK(fd >= 0, "the device is free again");
   if (fd >= 0) {
      struct nvg_info i3;
      CHECK(call(NVG_IOC_INFO, &i3) == 0 && i3.vram_used_B == 0, "and the new session starts empty (VRAM in use %llu)", (unsigned long long)i3.vram_used_B);
      uint64_t o = 0;
      uint32_t h = bo_create(4096, NVG_BO_SYSTEM, &o);
      CHECK(h == 1 && o == 0, "handles and arena start over (%u, %#llx)", h, (unsigned long long)o);
      close(fd);
   }
   fd = open("/dev/nvgpu", O_RDWR);
   CHECK(fd >= 0, "closing frees it too");
   if (fd >= 0) close(fd);

   if (failures) { printf("nvgpu_sw_test: %d FAILED\n", failures); return 1; }
   printf("nvgpu_sw_test: OK\n");
   return 0;
}
