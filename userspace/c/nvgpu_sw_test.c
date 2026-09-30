// /dev/nvgpu, the software device (G4a): the whole interface of nvgpu/uapi/nvgpu.h, checked from user space. Bookkeeping and memory are
// real; execution is not, so this test proves what Mesa's backend will rely on: sessions (several opens at once, each with its own
// slice of the VA space and its own handles, VRAM from one shared heap, at most 12 at a time), BOs mapped through the arena, VA
// allocation and binding with its error cases, contexts, EXEC with timeline waits and signals, and a session being handed back
// when its last holder dies, and sharing BOs and timelines between sessions (BO_EXPORT / BO_IMPORT, SYNC_EXPORT / SYNC_IMPORT, SCM_RIGHTS, lifetimes).
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <signal.h>
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

extern int ioctl(int fd, unsigned long request, ...);

// A raw syscall: mlibc has no syscall(), and the point is to control the register the request travels in.
static long sc3(long nr, long a, long b, long c) {
   long ret;
   __asm__ volatile("syscall" : "=a"(ret) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
   return ret;
}

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

static int sync_query2(uint32_t h, uint64_t *v, uint64_t *pending) {
   struct nvg_sync_query q = { .handle = h };
   int r = call(NVG_IOC_SYNC_QUERY, &q);
   *v = q.value;
   if (pending) *pending = q.pending;
   return r;
}

static int sync_query(uint32_t h, uint64_t *v) { return sync_query2(h, v, NULL); }

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

// ---- passing a descriptor over a socketpair (SCM_RIGHTS) -----------------------------------------------------------------------

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

static uint64_t vram_in_use(int dev) {
   struct nvg_info i;
   return ioctl(dev, NVG_IOC_INFO, &i) == 0 ? i.vram_used_B : ~0ull;
}

/// What sharing keeps alive, from /proc/kdebug's `gpu_share:` line: live storage allocations and shared timelines (and sessions).
static int share_counts(int *sessions, int *allocs, int *syncs) {
   FILE *f = fopen("/proc/kdebug", "r");
   if (!f) return -1;
   char line[512];
   int found = -1;
   while (fgets(line, sizeof line, f)) {
      if (sscanf(line, "gpu_share: sessions=%d storage_allocs=%d syncs=%d", sessions, allocs, syncs) == 3) { found = 0; break; }
   }
   fclose(f);
   return found;
}

static uint64_t tl_value(int dev, uint32_t handle) {
   struct nvg_sync_query q = { .handle = handle };
   return ioctl(dev, NVG_IOC_SYNC_QUERY, &q) == 0 ? q.value : ~0ull;
}

static uint32_t tl_import(int dev, int sfd) {
   struct nvg_sync_import im = { .fd = sfd };
   return ioctl(dev, NVG_IOC_SYNC_IMPORT, &im) == 0 ? im.handle : 0;
}

int main(void) {
   printf("nvgpu_sw_test:\n");

   // ---- open, sessions
   fd = open("/dev/nvgpu", O_RDWR);
   CHECK(fd >= 0, "open /dev/nvgpu");
   if (fd < 0) return 1;

   struct nvg_info info;
   memset(&info, 0xff, sizeof info);
   CHECK(call(NVG_IOC_INFO, &info) == 0, "INFO");
   CHECK(info.abi_version == NVG_ABI_VERSION && (info.flags & NVG_INFO_SOFTWARE), "abi %u flags %#x", info.abi_version, info.flags);
   CHECK(info.sm == 86 && info.cls_compute == 0xc7c0 && info.bar_size_B == 0, "sm %u compute %#x bar %llu", info.sm, info.cls_compute,
         (unsigned long long)info.bar_size_B);
   CHECK(info.va_start < info.va_end && info.vram_size_B > 0 && strlen(info.device_name) > 0, "ranges and name");
   CHECK(call(0xc0004e77, &info) < 0 && errno == ENOTTY, "an unknown request is ENOTTY");
   // ---- a second open is a session of its own (G5 layer 1)
   {
      int fd2 = open("/dev/nvgpu", O_RDWR);
      CHECK(fd2 >= 0, "a second open gets its own session (errno %d)", errno);
      if (fd2 >= 0) {
         struct nvg_info a, b;
         CHECK(ioctl(fd, NVG_IOC_INFO, &a) == 0 && ioctl(fd2, NVG_IOC_INFO, &b) == 0, "INFO on both");
         CHECK(a.va_end <= b.va_start || b.va_end <= a.va_start, "disjoint VA ranges [%#llx,%#llx) [%#llx,%#llx)", (unsigned long long)a.va_start,
               (unsigned long long)a.va_end, (unsigned long long)b.va_start, (unsigned long long)b.va_end);
         CHECK(a.va_end - a.va_start == (16ull << 30) && b.va_end - b.va_start == (16ull << 30), "16 GiB each");
         CHECK(a.va_start >= (1ull << 36) && b.va_end <= (1ull << 38), "inside [64 GiB, 256 GiB)");
         // handles are per session: a BO made on fd2 is nothing to fd
         struct nvg_bo_create c = { .size = 1 << 20, .flags = NVG_BO_VRAM };
         CHECK(ioctl(fd2, NVG_IOC_BO_CREATE, &c) == 0, "a VRAM BO on the second session");
         struct nvg_bo_free f = { .handle = c.handle };
         CHECK(ioctl(fd, NVG_IOC_BO_FREE, &f) < 0 && errno == ENOENT, "the first session does not have it (errno %d)", errno);
         // the VRAM heap is shared: what the second session holds shows in the first one's INFO
         struct nvg_info a2;
         CHECK(ioctl(fd, NVG_IOC_INFO, &a2) == 0 && a2.vram_used_B == a.vram_used_B + (1 << 20), "shared VRAM accounting (%llu -> %llu)",
               (unsigned long long)a.vram_used_B, (unsigned long long)a2.vram_used_B);
         // nor is the other session's address space
         struct nvg_va_alloc va = { .size = 0x10000, .align = 0x10000 };
         CHECK(ioctl(fd2, NVG_IOC_VA_ALLOC, &va) == 0 && va.va >= b.va_start && va.va + 0x10000 <= b.va_end, "VA alloc on the second session lands in its slice");
         struct nvg_bo_create mine = { .size = 0x10000, .flags = NVG_BO_SYSTEM };
         CHECK(ioctl(fd, NVG_IOC_BO_CREATE, &mine) == 0, "a BO on the first session");
         struct nvg_va_bind vb = { .va = va.va, .size = 0x10000, .bo_offset = 0, .handle = mine.handle };
         CHECK(ioctl(fd, NVG_IOC_VA_BIND, &vb) < 0 && errno == EINVAL, "binding at the other session's address is EINVAL (errno %d)", errno);
         struct nvg_bo_free f1 = { .handle = mine.handle };
         ioctl(fd, NVG_IOC_BO_FREE, &f1);
         close(fd2);
         CHECK(ioctl(fd, NVG_IOC_INFO, &a2) == 0 && a2.vram_used_B == a.vram_used_B, "closing the session gives its VRAM back (%llu)",
               (unsigned long long)a2.vram_used_B);
         // and its slot: the next open gets the lowest free one again, which is the same range
         int fd3 = open("/dev/nvgpu", O_RDWR);
         struct nvg_info c3;
         CHECK(fd3 >= 0 && ioctl(fd3, NVG_IOC_INFO, &c3) == 0 && c3.va_start == b.va_start, "the freed slot is reused");
         if (fd3 >= 0) close(fd3);
      }
   }

   // musl's ioctl() takes an int, so the request reaches the kernel sign-extended to 64 bits: only the low 32 bits count (as in Linux)
   struct nvg_info wide;
   memset(&wide, 0, sizeof wide);
   long wr = sc3(16 /* ioctl */, fd, (long)(int)NVG_IOC_INFO, (long)&wide);
   CHECK(wr == 0 && wide.abi_version == NVG_ABI_VERSION, "a sign-extended request works (%ld, abi %u)", wr, wide.abi_version);

   // ---- sharing BOs between sessions (G5 layer 2)
   {
      int fd2 = open("/dev/nvgpu", O_RDWR);
      CHECK(fd2 >= 0, "a second session to share with");
      if (fd2 >= 0) {
         // a system BO written by the first session, exported, imported by the second: the same memory under another handle
         struct nvg_bo_create c = { .size = 8192, .flags = NVG_BO_SYSTEM };
         CHECK(ioctl(fd, NVG_IOC_BO_CREATE, &c) == 0, "a system BO on the first session");
         unsigned char *ma = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, c.mmap_offset);
         CHECK(mapped(ma), "mapped there");
         memset(ma, 0x3c, 8192);
         struct nvg_bo_export ex = { .handle = c.handle, .flags = 0 };
         int efd = ioctl(fd, NVG_IOC_BO_EXPORT, &ex);
         CHECK(efd >= 0, "BO_EXPORT returns a descriptor (%d)", efd);
         CHECK(efd >= 0 && (fcntl(efd, F_GETFD) & FD_CLOEXEC), "the descriptor is close-on-exec");
         struct nvg_bo_import im = { .fd = efd };
         CHECK(ioctl(fd2, NVG_IOC_BO_IMPORT, &im) == 0, "BO_IMPORT on the second session");
         CHECK(im.handle != 0 && im.size_out == 8192 && im.mmap_offset == c.mmap_offset, "same size and arena offset (%llu, %#llx vs %#llx)",
               (unsigned long long)im.size_out, (unsigned long long)im.mmap_offset, (unsigned long long)c.mmap_offset);
         unsigned char *mb = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd2, im.mmap_offset);
         CHECK(mapped(mb) && mb[0] == 0x3c && mb[8191] == 0x3c, "the importer sees what the exporter wrote");
         if (mapped(ma) && mapped(mb)) {
            mb[100] = 0x77;
            ma[200] = 0x55;
            CHECK(ma[100] == 0x77 && mb[200] == 0x55, "writes go both ways: it is the same memory");
         }
         // the import is a BO like any other: it binds (in this session's own range), and freeing it is per session
         struct nvg_info i2;
         ioctl(fd2, NVG_IOC_INFO, &i2);
         struct nvg_va_alloc va = { .size = 8192, .align = 4096 };
         CHECK(ioctl(fd2, NVG_IOC_VA_ALLOC, &va) == 0, "a VA range on the importer");
         struct nvg_va_bind vb = { .va = va.va, .size = 8192, .bo_offset = 0, .handle = im.handle };
         CHECK(ioctl(fd2, NVG_IOC_VA_BIND, &vb) == 0, "the imported BO binds");
         struct nvg_va_unbind vu = { .va = va.va, .size = 8192 };
         ioctl(fd2, NVG_IOC_VA_UNBIND, &vu);
         struct nvg_va_free vf = { .va = va.va, .size = 8192 };
         ioctl(fd2, NVG_IOC_VA_FREE, &vf);
         // an import gives a handle of its own, every time
         struct nvg_bo_import im2 = { .fd = efd };
         CHECK(ioctl(fd2, NVG_IOC_BO_IMPORT, &im2) == 0 && im2.handle != im.handle, "importing twice gives two handles (%u %u)", im.handle, im2.handle);
         struct nvg_bo_free f2 = { .handle = im2.handle };
         CHECK(ioctl(fd2, NVG_IOC_BO_FREE, &f2) == 0, "and each can be freed on its own");

         // the refusals
         struct nvg_bo_export bad_ex = { .handle = 4242, .flags = 0 };
         CHECK(ioctl(fd, NVG_IOC_BO_EXPORT, &bad_ex) < 0 && errno == ENOENT, "exporting a BO that does not exist is ENOENT (errno %d)", errno);
         bad_ex = (struct nvg_bo_export){ .handle = c.handle, .flags = 1 };
         CHECK(ioctl(fd, NVG_IOC_BO_EXPORT, &bad_ex) < 0 && errno == EINVAL, "export flags must be 0 (errno %d)", errno);
         struct nvg_bo_import bad = { .fd = -1 };
         CHECK(ioctl(fd2, NVG_IOC_BO_IMPORT, &bad) < 0 && errno == EBADF, "import of fd -1 is EBADF (errno %d)", errno);
         bad = (struct nvg_bo_import){ .fd = 200 };
         CHECK(ioctl(fd2, NVG_IOC_BO_IMPORT, &bad) < 0 && errno == EBADF, "import of a descriptor that is not open is EBADF (errno %d)", errno);
         int plain = open("/dev/null", O_RDWR);
         bad = (struct nvg_bo_import){ .fd = plain };
         CHECK(ioctl(fd2, NVG_IOC_BO_IMPORT, &bad) < 0 && errno == EINVAL, "import of a descriptor that is not a BO is EINVAL (errno %d)", errno);
         close(plain);
         bad = (struct nvg_bo_import){ .fd = fd };
         CHECK(ioctl(fd2, NVG_IOC_BO_IMPORT, &bad) < 0 && errno == EINVAL, "the device's own descriptor is not a BO either (errno %d)", errno);
         bad = (struct nvg_bo_import){ .fd = efd, .flags = 1 };
         CHECK(ioctl(fd2, NVG_IOC_BO_IMPORT, &bad) < 0 && errno == EINVAL, "import flags must be 0 (errno %d)", errno);

         // lifetimes, with VRAM so the heap's use says who still holds what
         uint64_t base = vram_in_use(fd);
         struct nvg_bo_create cv = { .size = 1 << 20, .flags = NVG_BO_VRAM };
         CHECK(ioctl(fd, NVG_IOC_BO_CREATE, &cv) == 0 && vram_in_use(fd) == base + (1 << 20), "a 1 MiB VRAM BO");
         struct nvg_bo_export ev = { .handle = cv.handle };
         int vfd = ioctl(fd, NVG_IOC_BO_EXPORT, &ev);
         struct nvg_bo_free fv = { .handle = cv.handle };
         CHECK(ioctl(fd, NVG_IOC_BO_FREE, &fv) == 0 && vram_in_use(fd) == base + (1 << 20), "BO_FREE while a descriptor exists keeps the memory");
         int vdup = dup(vfd);
         close(vfd);
         CHECK(vram_in_use(fd) == base + (1 << 20), "a dup keeps it after the original descriptor closes");
         struct nvg_bo_import iv = { .fd = vdup };
         CHECK(ioctl(fd2, NVG_IOC_BO_IMPORT, &iv) == 0 && iv.mmap_offset == ~0ull && iv.size_out == (1 << 20), "a VRAM BO imports (no CPU mapping: ~0)");
         close(vdup);
         CHECK(vram_in_use(fd) == base + (1 << 20), "the importer's handle holds it once every descriptor is closed");
         struct nvg_bo_free fiv = { .handle = iv.handle };
         CHECK(ioctl(fd2, NVG_IOC_BO_FREE, &fiv) == 0 && vram_in_use(fd) == base, "and the last holder letting go gives it back (%llu vs %llu)",
               (unsigned long long)vram_in_use(fd), (unsigned long long)base);
         // a descriptor nobody imports holds the memory by itself
         struct nvg_bo_create cw = { .size = 1 << 20, .flags = NVG_BO_VRAM };
         ioctl(fd, NVG_IOC_BO_CREATE, &cw);
         struct nvg_bo_export ew = { .handle = cw.handle };
         int wfd = ioctl(fd, NVG_IOC_BO_EXPORT, &ew);
         struct nvg_bo_free fw = { .handle = cw.handle };
         ioctl(fd, NVG_IOC_BO_FREE, &fw);
         CHECK(wfd >= 0 && vram_in_use(fd) == base + (1 << 20), "an exported descriptor alone keeps the memory");
         close(wfd);
         CHECK(vram_in_use(fd) == base, "closing it frees the memory");

         // a full descriptor table: the export fails with EMFILE and the hold it took is given back (nothing leaks)
         {
            struct nvg_bo_create cf = { .size = 1 << 20, .flags = NVG_BO_VRAM };
            ioctl(fd, NVG_IOC_BO_CREATE, &cf);
            int filler[300], nf = 0;
            while (nf < 300 && (filler[nf] = open("/dev/null", O_RDWR)) >= 0) nf++;
            struct nvg_bo_export ef = { .handle = cf.handle };
            int r = ioctl(fd, NVG_IOC_BO_EXPORT, &ef);
            CHECK(r < 0 && errno == EMFILE, "with the descriptor table full, BO_EXPORT is EMFILE (%d, errno %d, after %d opens)", r, errno, nf);
            for (int k = 0; k < nf; k++) close(filler[k]);
            struct nvg_bo_free ff = { .handle = cf.handle };
            ioctl(fd, NVG_IOC_BO_FREE, &ff);
            CHECK(vram_in_use(fd) == base, "and the failed export holds nothing (%llu vs %llu)", (unsigned long long)vram_in_use(fd), (unsigned long long)base);
         }

         // the exporter's whole session ends while the importer still uses a system BO: the pages stay, then go with the importer
         struct nvg_bo_free fo = { .handle = c.handle };
         ioctl(fd, NVG_IOC_BO_FREE, &fo);
         close(efd);
         close(fd);
         fd = -1;
         CHECK(mapped(mb) && mb[0] == 0x3c && mb[8191] == 0x3c, "the exporter is gone (session closed, handle freed, descriptor closed): the importer's pages are intact");
         struct nvg_bo_free fi = { .handle = im.handle };
         CHECK(ioctl(fd2, NVG_IOC_BO_FREE, &fi) == 0, "the importer frees its handle");
         struct nvg_bo_create again = { .size = 8192, .flags = NVG_BO_SYSTEM };
         CHECK(ioctl(fd2, NVG_IOC_BO_CREATE, &again) == 0 && again.mmap_offset == c.mmap_offset, "the range is free for reuse (%#llx)", (unsigned long long)again.mmap_offset);
         unsigned char *mc = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd2, again.mmap_offset);
         CHECK(mapped(mc) && mc[0] == 0 && mc[8191] == 0 && mc[100] == 0, "and a reused range reads as zeros: the old pages were given back");
         close(fd2);
         fd = open("/dev/nvgpu", O_RDWR);
         CHECK(fd >= 0, "the first session's slot is free again");
         if (fd < 0) return 1;
         CHECK(vram_in_use(fd) == 0, "nothing is held after every session closed (%llu)", (unsigned long long)vram_in_use(fd));
         {
            int ns = -1, na = -1, ny = -1;
            CHECK(share_counts(&ns, &na, &ny) == 0 && na == 0 && ny == 0, "and no storage allocation or shared timeline is alive (sessions %d, allocs %d, syncs %d)", ns, na, ny);
         }
      }
   }

   // ---- a descriptor travels by SCM_RIGHTS to another process, which imports it into its own session
   {
      int sv[2];
      CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, sv) == 0, "a socketpair");
      struct nvg_bo_create c = { .size = 4096, .flags = NVG_BO_SYSTEM };
      ioctl(fd, NVG_IOC_BO_CREATE, &c);
      unsigned char *m = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, c.mmap_offset);
      CHECK(mapped(m), "a system BO to send");
      memset(m, 0x21, 4096);
      struct nvg_bo_export ex = { .handle = c.handle };
      int efd = ioctl(fd, NVG_IOC_BO_EXPORT, &ex);
      pid_t child = fork();
      if (child == 0) {
         close(sv[0]);
         int got = recv_fd(sv[1]);
         int dev = open("/dev/nvgpu", O_RDWR);   // its own session
         if (got < 0 || dev < 0) _exit(10);
         struct nvg_bo_import im = { .fd = got };
         if (ioctl(dev, NVG_IOC_BO_IMPORT, &im) != 0) _exit(11);
         unsigned char *cm = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, dev, im.mmap_offset);
         if (cm == MAP_FAILED || cm[0] != 0x21 || cm[4095] != 0x21) _exit(12);
         cm[7] = 0x99;   // the parent must see this
         _exit(0);
      }
      close(sv[1]);
      CHECK(efd >= 0 && send_fd(sv[0], efd) == 0, "the descriptor is sent");
      close(efd);   // the child has (or will get) its own copy in flight; the BO's storage must survive this close
      int st = 0;
      waitpid(child, &st, 0);
      CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "the child imported it, saw the pattern and wrote to it (exit %d)", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
      CHECK(m[7] == 0x99 && m[0] == 0x21, "the parent sees the child's write (%#x)", m[7]);
      struct nvg_bo_free fr = { .handle = c.handle };
      ioctl(fd, NVG_IOC_BO_FREE, &fr);
      close(sv[0]);
      // a descriptor sent and never received is dropped with the socket: nothing leaks
      int sv2[2];
      socketpair(AF_UNIX, SOCK_STREAM, 0, sv2);
      struct nvg_bo_create cz = { .size = 1 << 20, .flags = NVG_BO_VRAM };
      ioctl(fd, NVG_IOC_BO_CREATE, &cz);
      struct nvg_bo_export ez = { .handle = cz.handle };
      int zfd = ioctl(fd, NVG_IOC_BO_EXPORT, &ez);
      send_fd(sv2[0], zfd);
      close(zfd);
      struct nvg_bo_free fz = { .handle = cz.handle };
      ioctl(fd, NVG_IOC_BO_FREE, &fz);
      CHECK(vram_in_use(fd) == (1 << 20), "a descriptor in flight on a socket holds the memory");
      close(sv2[0]);
      close(sv2[1]);
      CHECK(vram_in_use(fd) == 0, "and closing the socket unread drops it (%llu)", (unsigned long long)vram_in_use(fd));
   }

   // ---- sharing timelines between sessions (G5 layer 2)
   {
      int fd2 = open("/dev/nvgpu", O_RDWR);
      CHECK(fd2 >= 0, "a second session for the timelines");
      if (fd2 >= 0) {
         struct nvg_sync_create sc = { .initial = 2 };
         CHECK(ioctl(fd, NVG_IOC_SYNC_CREATE, &sc) == 0, "a timeline on the first session");
         struct nvg_sync_export ex = { .handle = sc.handle };
         int sfd = ioctl(fd, NVG_IOC_SYNC_EXPORT, &ex);
         CHECK(sfd >= 0 && (fcntl(sfd, F_GETFD) & FD_CLOEXEC), "SYNC_EXPORT returns a close-on-exec descriptor (%d)", sfd);
         struct nvg_sync_export ex2 = { .handle = sc.handle };
         int sfd2 = ioctl(fd, NVG_IOC_SYNC_EXPORT, &ex2);
         CHECK(sfd2 >= 0 && sfd2 != sfd, "exporting again gives another descriptor (%d)", sfd2);
         uint32_t h2 = tl_import(fd2, sfd);
         CHECK(h2 != 0, "SYNC_IMPORT on the second session");
         CHECK(tl_value(fd2, h2) == 2, "it starts at the value the timeline had when it was shared (%llu)", (unsigned long long)tl_value(fd2, h2));
         // one timeline, two handles
         struct nvg_sync_signal sg = { .handle = h2, .value = 9 };
         CHECK(ioctl(fd2, NVG_IOC_SYNC_SIGNAL, &sg) == 0 && tl_value(fd, sc.handle) == 9, "a signal on the importer is the exporter's value (%llu)", (unsigned long long)tl_value(fd, sc.handle));
         struct nvg_sync_query q9 = { .handle = sc.handle };
         CHECK(ioctl(fd, NVG_IOC_SYNC_QUERY, &q9) == 0 && q9.pending >= 9, "and it raised the pending value too (%llu)", (unsigned long long)q9.pending);
         sg = (struct nvg_sync_signal){ .handle = sc.handle, .value = 4 };
         CHECK(ioctl(fd, NVG_IOC_SYNC_SIGNAL, &sg) < 0 && errno == EINVAL, "values do not go backwards on either side (errno %d)", errno);
         // EXEC signals on the shared timeline from the first session; the second session sees it complete (the software device completes at once)
         struct nvg_ctx_create cx = { .engines = NVG_ENGINE_COMPUTE };
         CHECK(ioctl(fd, NVG_IOC_CTX_CREATE, &cx) == 0, "a context to signal from");
         struct nvg_sync_ref sig = { .handle = sc.handle, .value = 12 };
         struct nvg_exec e = { .ctx = cx.ctx, .sig_count = 1, .signals = (uintptr_t)&sig };
         CHECK(ioctl(fd, NVG_IOC_EXEC, &e) == 0, "an EXEC signalling the shared timeline");
         struct nvg_sync_query q2 = { .handle = h2 };
         CHECK(ioctl(fd2, NVG_IOC_SYNC_QUERY, &q2) == 0 && q2.value == 12 && q2.pending >= 12, "the other session sees it (value %llu pending %llu)", (unsigned long long)q2.value, (unsigned long long)q2.pending);
         // and waits on it: satisfied, then not
         struct nvg_ctx_create cy = { .engines = NVG_ENGINE_COMPUTE };
         ioctl(fd2, NVG_IOC_CTX_CREATE, &cy);
         struct nvg_sync_ref w_ok = { .handle = h2, .value = 12 }, w_no = { .handle = h2, .value = 13 };
         struct nvg_exec ew = { .ctx = cy.ctx, .wait_count = 1, .waits = (uintptr_t)&w_ok };
         CHECK(ioctl(fd2, NVG_IOC_EXEC, &ew) == 0, "an EXEC on the second session waiting for 12 runs");
         ew.waits = (uintptr_t)&w_no;
         CHECK(ioctl(fd2, NVG_IOC_EXEC, &ew) < 0 && errno == EAGAIN, "waiting for 13 is EAGAIN (errno %d)", errno);

         // the refusals
         struct nvg_sync_export bad_ex = { .handle = 4242 };
         CHECK(ioctl(fd, NVG_IOC_SYNC_EXPORT, &bad_ex) < 0 && errno == ENOENT, "exporting a timeline that does not exist is ENOENT (errno %d)", errno);
         bad_ex = (struct nvg_sync_export){ .handle = sc.handle, .flags = 1 };
         CHECK(ioctl(fd, NVG_IOC_SYNC_EXPORT, &bad_ex) < 0 && errno == EINVAL, "export flags must be 0 (errno %d)", errno);
         struct nvg_sync_import bad = { .fd = -1 };
         CHECK(ioctl(fd2, NVG_IOC_SYNC_IMPORT, &bad) < 0 && errno == EBADF, "import of fd -1 is EBADF (errno %d)", errno);
         bad = (struct nvg_sync_import){ .fd = 200 };
         CHECK(ioctl(fd2, NVG_IOC_SYNC_IMPORT, &bad) < 0 && errno == EBADF, "import of a descriptor that is not open is EBADF (errno %d)", errno);
         int plain = open("/dev/null", O_RDWR);
         bad = (struct nvg_sync_import){ .fd = plain };
         CHECK(ioctl(fd2, NVG_IOC_SYNC_IMPORT, &bad) < 0 && errno == EINVAL, "a file is not a timeline (errno %d)", errno);
         close(plain);
         bad = (struct nvg_sync_import){ .fd = sfd, .flags = 1 };
         CHECK(ioctl(fd2, NVG_IOC_SYNC_IMPORT, &bad) < 0 && errno == EINVAL, "import flags must be 0 (errno %d)", errno);
         // a BO descriptor is not a timeline and a timeline descriptor is not a BO
         struct nvg_bo_create bc = { .size = 4096, .flags = NVG_BO_SYSTEM };
         ioctl(fd, NVG_IOC_BO_CREATE, &bc);
         struct nvg_bo_export be = { .handle = bc.handle };
         int bfd = ioctl(fd, NVG_IOC_BO_EXPORT, &be);
         bad = (struct nvg_sync_import){ .fd = bfd };
         CHECK(bfd >= 0 && ioctl(fd2, NVG_IOC_SYNC_IMPORT, &bad) < 0 && errno == EINVAL, "a BO descriptor is not a timeline (errno %d)", errno);
         struct nvg_bo_import bi = { .fd = sfd };
         CHECK(ioctl(fd2, NVG_IOC_BO_IMPORT, &bi) < 0 && errno == EINVAL, "and a timeline descriptor is not a BO (errno %d)", errno);
         close(bfd);
         struct nvg_bo_free bf = { .handle = bc.handle };
         ioctl(fd, NVG_IOC_BO_FREE, &bf);

         // lifetimes: the exporter's session ends, its handle is gone; the timeline is still there for a later importer, through the descriptor
         struct nvg_sync_destroy sd = { .handle = sc.handle };
         CHECK(ioctl(fd, NVG_IOC_SYNC_DESTROY, &sd) == 0, "the exporter destroys its handle");
         close(sfd);
         close(fd);
         fd = -1;
         CHECK(tl_value(fd2, h2) == 12, "the importer still has the timeline (%llu)", (unsigned long long)tl_value(fd2, h2));
         uint32_t h3 = tl_import(fd2, sfd2);
         CHECK(h3 != 0 && tl_value(fd2, h3) == 12, "and a later import through the other descriptor finds it at 12 (handle %u)", h3);
         close(sfd2);
         struct nvg_sync_destroy sd2 = { .handle = h2 };
         ioctl(fd2, NVG_IOC_SYNC_DESTROY, &sd2);
         CHECK(tl_value(fd2, h3) == 12, "with the descriptors closed and one handle destroyed, the other handle keeps it");
         close(fd2);
         fd = open("/dev/nvgpu", O_RDWR);
         CHECK(fd >= 0, "the first session's slot is free again");
         if (fd < 0) return 1;
         {
            int ns = -1, na = -1, ny = -1;
            CHECK(share_counts(&ns, &na, &ny) == 0 && ny == 0 && na == 0 && ns == 1, "every shared timeline is gone with its last holder (sessions %d, allocs %d, syncs %d)", ns, na, ny);
         }
      }
   }

   // ---- a timeline descriptor sent to another process: its signal is seen by the sender, who does not call in until it looks
   {
      int sv[2];
      socketpair(AF_UNIX, SOCK_STREAM, 0, sv);
      struct nvg_sync_create sc = { .initial = 0 };
      ioctl(fd, NVG_IOC_SYNC_CREATE, &sc);
      struct nvg_sync_export ex = { .handle = sc.handle };
      int sfd = ioctl(fd, NVG_IOC_SYNC_EXPORT, &ex);
      pid_t child = fork();
      if (child == 0) {
         close(sv[0]);
         int got = recv_fd(sv[1]);
         int dev = open("/dev/nvgpu", O_RDWR);
         if (got < 0 || dev < 0) _exit(10);
         uint32_t h = tl_import(dev, got);
         if (!h) _exit(11);
         struct nvg_ctx_create cx = { .engines = NVG_ENGINE_COMPUTE };
         struct nvg_sync_ref sig = { .handle = h, .value = 5 };
         struct nvg_exec e = { .sig_count = 1, .signals = (uintptr_t)&sig };
         if (ioctl(dev, NVG_IOC_CTX_CREATE, &cx) != 0) _exit(12);
         e.ctx = cx.ctx;
         if (ioctl(dev, NVG_IOC_EXEC, &e) != 0) _exit(13);
         _exit(0);
      }
      close(sv[1]);
      CHECK(sfd >= 0 && send_fd(sv[0], sfd) == 0, "the timeline descriptor is sent");
      close(sfd);
      int st = 0;
      waitpid(child, &st, 0);
      CHECK(WIFEXITED(st) && WEXITSTATUS(st) == 0, "the child imported it and signalled from its own session (exit %d)", WIFEXITED(st) ? WEXITSTATUS(st) : -1);
      CHECK(tl_value(fd, sc.handle) == 5, "and the parent, which did nothing meanwhile, finds it at 5 (%llu)", (unsigned long long)tl_value(fd, sc.handle));
      struct nvg_sync_destroy sd = { .handle = sc.handle };
      ioctl(fd, NVG_IOC_SYNC_DESTROY, &sd);
      close(sv[0]);
      {
         int ns = -1, na = -1, ny = -1;
         CHECK(share_counts(&ns, &na, &ny) == 0 && ny == 0, "the child's session is gone and so is the timeline (sessions %d, syncs %d)", ns, ny);
      }
   }

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

   CHECK(va_bind_call(va, 8192, b3, 0) == 0, "bind 2 pages");
   CHECK(va_bind_call(va + 4096, 4096, b3, 0) < 0 && errno == EEXIST, "an overlapping bind is EEXIST");
   CHECK(va_bind_call(va + 1, 4096, b3, 0) < 0 && errno == EINVAL, "an unaligned bind is EINVAL");
   CHECK(va_bind_call(va + 0x8000, 12288, b3, 0) < 0 && errno == EINVAL, "past the end of the BO is EINVAL");
   CHECK(va_bind_call(va + 0x8000, 4096, 9999, 0) < 0 && errno == ENOENT, "an unknown BO is ENOENT");
   CHECK(va_bind_call(info.va_start - 4096, 4096, b3, 0) < 0 && errno == EINVAL, "outside any allocation is EINVAL");
   struct nvg_va_free vf = { .va = va, .size = 0x10000 };
   CHECK(call(NVG_IOC_VA_FREE, &vf) < 0 && errno == EBUSY, "freeing a range with bindings is EBUSY");
   CHECK(unbind(va + 4096, 4096) == 0, "unbind the second page (a cut)");
   CHECK(va_bind_call(va + 4096, 4096, b3, 4096) == 0, "and bind it again");
   CHECK(unbind(va, 0x10000) == 0, "unbind everything (gaps are fine)");
   CHECK(call(NVG_IOC_VA_FREE, &vf) == 0, "VA_FREE once nothing is bound");
   CHECK(call(NVG_IOC_VA_FREE, &vf) < 0 && errno == ENOENT, "VA_FREE twice is ENOENT");

   // ---- a BO closed while bound stays usable by the GPU until unbound
   uint64_t va2 = va_alloc(0x4000, 4096);
   struct nvg_bo_free fr3 = { .handle = b3 };
   CHECK(va_bind_call(va2, 8192, b3, 0) == 0 && call(NVG_IOC_BO_FREE, &fr3) == 0, "close a bound BO");
   CHECK(va_bind_call(va2 + 8192, 4096, b3, 0) < 0 && errno == ENOENT, "its handle is dead");
   CHECK(unbind(va2, 0x4000) == 0, "unbind releases it");

   // ---- contexts, EXEC and timelines
   struct nvg_ctx_create cc = { .engines = 0 };
   CHECK(call(NVG_IOC_CTX_CREATE, &cc) < 0 && errno == EINVAL, "no engines is EINVAL");
   cc.engines = NVG_ENGINE_COMPUTE | NVG_ENGINE_COPY;
   CHECK(call(NVG_IOC_CTX_CREATE, &cc) == 0 && cc.ctx != 0, "CTX_CREATE");

   uint64_t offp = 0;
   uint32_t bp = bo_create(8192, NVG_BO_SYSTEM, &offp);
   uint64_t vp = va_alloc(0x4000, 4096);
   CHECK(va_bind_call(vp, 8192, bp, 0) == 0, "bind the push memory");
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

   uint32_t first = 77;
   uint64_t pend = 0;
   CHECK(sync_query2(s, &v, &pend) == 0 && v == 5 && pend == 5, "a settled timeline has pending == value (%llu, %llu)", (unsigned long long)v, (unsigned long long)pend);
   struct nvg_sync_ref past_pending = { .handle = s, .value = 6 };
   CHECK(sync_wait(&past_pending, 1, NVG_WAIT_PENDING, &first) < 0 && errno == EAGAIN, "a value nobody will signal is not pending either");
   CHECK(sync_wait(&sig, 1, NVG_WAIT_PENDING, &first) == 0, "a pending wait for a completed value is ready");

   struct nvg_sync_ref both[2] = { { .handle = s, .value = 5 }, { .handle = gate, .value = 2 } };
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

   // ---- a session is handed back when its last holder goes: by the time waitpid returns, the dead process's files are closed
   //      (Linux closes them in do_exit). Repeated, because the window it used to lose in was a few microseconds wide. The sessions are
   //      all taken first (12 of them), so "the forked child still holds it" is a full table and EBUSY, as it used to be for one.
   int others[16], nothers = 0;
   for (;;) {
      int x = open("/dev/nvgpu", O_RDWR);
      if (x < 0) {
         CHECK(errno == EBUSY, "the table of sessions is full with EBUSY (errno %d)", errno);
         break;
      }
      if (nothers == 16) { close(x); break; }
      others[nothers++] = x;
   }
   CHECK(nothers == 11, "12 sessions in all: this one and %d more", nothers);
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
   for (int k = 0; k < nothers; k++) close(others[k]);
   CHECK(late == 0, "the session was still busy right after waitpid in %d of 40 rounds", late);
   CHECK(fd >= 0, "the session is free again");
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
