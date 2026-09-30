// static-pie loading: an ET_DYN executable with no libc and no interpreter, like the ones musl's rcrt1 produces. The kernel loads it at a
// base of its choosing and applies no relocations, so this program does what rcrt1 does: applies its own R_X86_64_RELATIVE entries from
// _DYNAMIC, then checks the auxv it was given (AT_PHDR must be biased, AT_ENTRY must be where _start really is).
// Built by kernel/build.rs (DISK_PIE_PROGRAMS), not against mlibc.
typedef unsigned long u64;
typedef long i64;

static i64 sys3(i64 nr, i64 a, i64 b, i64 c) {
    i64 r;
    __asm__ volatile("syscall" : "=a"(r) : "a"(nr), "D"(a), "S"(b), "d"(c) : "rcx", "r11", "memory");
    return r;
}
static void out(const char *s) {
    u64 n = 0;
    while (s[n]) n++;
    sys3(1, 1, (i64)s, (i64)n);
}
static void hex(const char *label, u64 v) {
    char buf[64], *p = buf;
    for (const char *l = label; *l; l++) *p++ = *l;
    *p++ = ' '; *p++ = '0'; *p++ = 'x';
    for (int i = 60; i >= 0; i -= 4) *p++ = "0123456789abcdef"[(v >> i) & 15];
    *p++ = '\n'; *p = 0;
    out(buf);
}

enum { DT_NULL = 0, DT_RELA = 7, DT_RELASZ = 8, DT_RELAENT = 9 };
enum { R_X86_64_RELATIVE = 8 };
enum { AT_NULL_ = 0, AT_PHDR_ = 3, AT_PHENT_ = 4, AT_PHNUM_ = 5, AT_PAGESZ_ = 6, AT_ENTRY_ = 9, AT_RANDOM_ = 25 };
typedef struct { i64 tag; u64 val; } Dyn;
typedef struct { u64 off; u64 info; i64 addend; } Rela;
typedef struct { unsigned type, flags; u64 offset, vaddr, paddr, filesz, memsz, align; } Phdr;

extern char __ehdr_start[] __attribute__((visibility("hidden")));
extern Dyn _DYNAMIC[] __attribute__((visibility("hidden")));

// pointers in data: each needs a RELATIVE relocation to be valid
static const char msg_a[] = "  ok   pointer table entry a\n";
static const char msg_b[] = "  ok   pointer table entry b\n";
static const char *const table[] = { msg_a, msg_b };
static int counter = 41;
static int *const counter_ptr = &counter;

static int failures;
static void check(int cond, const char *what) {
    if (cond) { out("  ok   "); out(what); out("\n"); }
    else { failures++; out("  FAIL "); out(what); out("\n"); }
}

__attribute__((used)) static void start_c(u64 *sp) {
    u64 base = (u64)__ehdr_start;   // hidden, pc-relative: the address it really has, not the link-time 0
    Rela *rela = 0; u64 relasz = 0;
    for (Dyn *d = _DYNAMIC; d->tag != DT_NULL; d++) {
        if (d->tag == DT_RELA) rela = (Rela *)(base + d->val);
        else if (d->tag == DT_RELASZ) relasz = d->val;
    }
    for (u64 i = 0; i < relasz / sizeof(Rela); i++) {
        if ((rela[i].info & 0xffffffff) == R_X86_64_RELATIVE) *(u64 *)(base + rela[i].off) = base + rela[i].addend;
    }

    out("static-pie\n");
    hex("base", base);
    check(base >= 0x100000 && (base & 0xfff) == 0, "loaded above the first MiB, page-aligned"); // not `base != 0`: the address of an object is never null, so the compiler folds that away
    check(relasz != 0, "the file has relocations");
    out(table[0]);
    out(table[1]);
    check(*counter_ptr == 41, "a relocated data pointer reads its target");
    (*counter_ptr)++;
    check(counter == 42, "writing through it reaches the variable (.data is writable)");

    u64 argc = sp[0];
    u64 *envp = sp + 1 + argc + 1;
    while (*envp) envp++;
    u64 *aux = envp + 1;
    u64 phdr = 0, phent = 0, phnum = 0, pagesz = 0, entry = 0, random = 0;
    for (; aux[0] != AT_NULL_; aux += 2) {
        switch (aux[0]) {
        case AT_PHDR_: phdr = aux[1]; break;
        case AT_PHENT_: phent = aux[1]; break;
        case AT_PHNUM_: phnum = aux[1]; break;
        case AT_PAGESZ_: pagesz = aux[1]; break;
        case AT_ENTRY_: entry = aux[1]; break;
        case AT_RANDOM_: random = aux[1]; break;
        }
    }
    hex("AT_PHDR", phdr);
    hex("AT_ENTRY", entry);
    check(phent == sizeof(Phdr), "AT_PHENT");
    check(phnum >= 3, "AT_PHNUM");
    check(pagesz == 4096, "AT_PAGESZ");
    extern void _start(void);
    check(entry == (u64)_start, "AT_ENTRY is the biased entry point");
    check(phdr >= base && phdr < base + 0x100000, "AT_PHDR is inside the image (biased)");
    int loads = 0, dynamic = 0;
    for (u64 i = 0; i < phnum; i++) {
        Phdr *p = (Phdr *)(phdr + i * phent);
        loads += p->type == 1;
        if (p->type == 2) dynamic = base + p->vaddr == (u64)_DYNAMIC;
    }
    check(loads >= 1 && dynamic, "walking AT_PHDR finds PT_LOAD and PT_DYNAMIC at the right address");
    check(random != 0, "AT_RANDOM");
    if (random) {
        unsigned char *r = (unsigned char *)random;
        int nz = 0;
        for (int i = 0; i < 16; i++) nz |= r[i];
        check(nz != 0, "AT_RANDOM bytes are not all zero");
    }
    out(failures ? "pie_test: FAILED\n" : "pie_test: OK\n");
    sys3(60, failures != 0, 0, 0);
    for (;;) {}
}

__asm__(".globl _start\n_start:\n  xor %ebp,%ebp\n  mov %rsp,%rdi\n  and $-16,%rsp\n  call start_c\n  ud2\n");
