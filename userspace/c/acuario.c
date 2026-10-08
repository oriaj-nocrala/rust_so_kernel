// acuario: the scheduler as a fish tank.
//
// Each CPU is a lane of water and each process is a fish swimming in the
// lane of the CPU it last ran on (`/proc/<pid>/stat` field 39). A fish that
// got CPU time since the last sample swims fast and blows bubbles; a
// sleeping one drifts and snores; a stopped one freezes; a zombie floats
// belly-up; a process that exits bursts into bubbles. When the scheduler
// migrates a process the fish swims across lanes, leaving a splash, and the
// header counts the migrations seen. The size of the fish is its RSS.
//
// Keys: q quit, f feed (fork a child that burns CPU for a few seconds),
// F a school (one hungry child per CPU), n names on/off, space pause.
//
// Everything it shows comes from /proc/stat and /proc/<pid>/stat, so it is
// also a cheap live check of both and of fork/exit/migration.
#include <dirent.h>
#include <poll.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#define MAXCPU 64
#define MAXFISH 256
#define MAXBUB 512
#define MAXROWS 120
#define MAXCOLS 300
#define FRAME_MS 80
#define SAMPLE_EVERY 4 // frames between two /proc samples

struct cell { char ch; unsigned char fg, bg, bold; };

struct fish {
    int used, pid, seen;
    char name[17];
    char state;
    unsigned long long ticks;
    long delta; // CPU ticks gained in the last sample
    long rss;   // pages
    int cpu;
    float x, ay; // column, absolute row (eased towards the lane)
    int fy;      // row within the lane
    float vx;
    int age;
    unsigned char color;
};

struct bubble { int used; float x, y, vy; int life; char ch; };

static struct cell scr[MAXROWS][MAXCOLS];
static struct fish fish[MAXFISH];
static struct bubble bub[MAXBUB];
static int rows = 25, cols = 80, ncpu = 1, lane_h = 1, nlanes = 1, top = 1;
static unsigned long long cpu_busy[MAXCPU], cpu_total[MAXCPU];
static int cpu_pct[MAXCPU];
static long migrations, births, deaths;
static int show_names = 1, paused;
static int self_pid;
static volatile sig_atomic_t quit;
static struct termios saved_tio;
static unsigned rng = 0x9e3779b9;

static unsigned rnd(void) { rng ^= rng << 13; rng ^= rng >> 17; rng ^= rng << 5; return rng; }
static float frand(void) { return (rnd() & 0xffff) / 65536.0f; }

static void on_signal(int sig) { (void)sig; quit = 1; }

// ---------------------------------------------------------------- /proc

static int read_file(const char *path, char *buf, int len) {
    FILE *f = fopen(path, "r");
    if (!f) return -1;
    int n = fread(buf, 1, len - 1, f);
    fclose(f);
    if (n < 0) n = 0;
    buf[n] = 0;
    return n;
}

static void sample_cpus(void) {
    static char buf[8192];
    if (read_file("/proc/stat", buf, sizeof buf) < 0) return;
    int n = 0;
    for (char *line = buf; line && *line; ) {
        char *next = strchr(line, '\n');
        if (next) *next++ = 0;
        if (!strncmp(line, "cpu", 3) && line[3] >= '0' && line[3] <= '9') {
            int c = atoi(line + 3);
            char *p = strchr(line, ' ');
            unsigned long long v[10] = {0}, total = 0;
            for (int i = 0; i < 10 && p && *p; i++) { v[i] = strtoull(p, &p, 10); total += v[i]; }
            unsigned long long idle = v[3] + v[4];
            if (c < MAXCPU) {
                unsigned long long dt = total - cpu_total[c], db = (total - idle) - cpu_busy[c];
                cpu_pct[c] = dt ? (int)(db * 100 / dt) : 0;
                cpu_total[c] = total;
                cpu_busy[c] = total - idle;
                if (c + 1 > n) n = c + 1;
            }
        }
        line = next;
    }
    if (n > 0) ncpu = n;
}

static struct fish *find_fish(int pid) {
    for (int i = 0; i < MAXFISH; i++) if (fish[i].used && fish[i].pid == pid) return &fish[i];
    return NULL;
}

static int lane_of(int cpu) { return cpu < nlanes ? cpu : nlanes - 1; }
static int lane_row(int cpu, int fy) { return top + lane_of(cpu) * lane_h + fy; }

static int pick_fy(void) {
    if (lane_h <= 1) return 0;
    if (lane_h == 2) return 1;
    return 1 + rnd() % (lane_h - 2); // the last row is the sea bed
}

static void add_bubbles(float x, float y, int n, int life) {
    for (int k = 0; k < n; k++)
        for (int i = 0; i < MAXBUB; i++) if (!bub[i].used) {
            const char *chs = ".oO*";
            bub[i] = (struct bubble){1, x + (frand() - 0.5f) * 4, y, 0.15f + frand() * 0.3f,
                                     life + rnd() % 6, chs[rnd() % 4]};
            break;
        }
}

static unsigned char color_for(const char *name) {
    static const unsigned char pal[] = {214, 226, 207, 51, 118, 177, 215, 159, 220, 213, 123, 229};
    unsigned h = 5381;
    for (const char *p = name; *p; p++) h = h * 33 + (unsigned char)*p;
    return pal[h % sizeof pal];
}

static void sample_procs(int gen) {
    DIR *d = opendir("/proc");
    if (!d) return;
    struct dirent *e;
    char path[64], buf[1024];
    while ((e = readdir(d))) {
        if (e->d_name[0] < '1' || e->d_name[0] > '9') continue;
        int pid = atoi(e->d_name);
        snprintf(path, sizeof path, "/proc/%d/stat", pid);
        if (read_file(path, buf, sizeof buf) <= 0) continue;
        char *lp = strchr(buf, '('), *rp = strrchr(buf, ')');
        if (!lp || !rp || rp[1] != ' ') continue;
        char name[17] = {0};
        int nl = rp - lp - 1; if (nl > 16) nl = 16; if (nl < 0) nl = 0;
        memcpy(name, lp + 1, nl);
        char state = rp[2];
        // Fields from 4 (ppid) on; we want 14 utime, 15 stime, 24 rss, 39 cpu.
        char *p = rp + 3;
        unsigned long long f[40] = {0};
        for (int i = 4; i < 40 && *p; i++) f[i] = strtoull(p, &p, 10);
        unsigned long long ticks = f[14] + f[15];
        int cpu = (int)f[39];
        if (cpu < 0 || cpu >= MAXCPU) cpu = 0;

        struct fish *fs = find_fish(pid);
        if (!fs) {
            for (int i = 0; i < MAXFISH; i++) if (!fish[i].used) { fs = &fish[i]; break; }
            if (!fs) continue;
            memset(fs, 0, sizeof *fs);
            fs->used = 1; fs->pid = pid; fs->ticks = ticks; fs->cpu = cpu;
            fs->x = 2 + frand() * (cols - 12);
            fs->fy = pick_fy();
            fs->ay = lane_row(cpu, fs->fy);
            fs->vx = frand() < 0.5f ? -0.2f : 0.2f;
            if (gen > 0) births++;
        }
        if (strcmp(fs->name, name)) { memcpy(fs->name, name, 17); fs->color = color_for(name); }
        fs->delta = (long)(ticks - fs->ticks);
        fs->ticks = ticks;
        fs->state = state;
        fs->rss = (long)f[24];
        if (cpu != fs->cpu && gen > 0) {
            add_bubbles(fs->x + 2, fs->ay, 5, 4);
            fs->cpu = cpu;
            fs->fy = pick_fy();
            migrations++;
        }
        fs->seen = gen;
    }
    closedir(d);
    for (int i = 0; i < MAXFISH; i++)
        if (fish[i].used && fish[i].seen != gen) {
            add_bubbles(fish[i].x + 3, fish[i].ay, 10, 10);
            fish[i].used = 0;
            deaths++;
        }
}

// ---------------------------------------------------------------- drawing

static void put(int r, int c, char ch, unsigned char fg, int bold) {
    if (r < 0 || r >= rows || c < 0 || c >= cols) return;
    scr[r][c].ch = ch; scr[r][c].fg = fg; scr[r][c].bold = bold;
}

static void puts_at(int r, int c, const char *s, unsigned char fg, int bold) {
    for (; *s; s++, c++) put(r, c, *s, fg, bold);
}

static void put_bg(int r, int c, unsigned char bg) {
    if (r >= 0 && r < rows && c >= 0 && c < cols) scr[r][c].bg = bg;
}

static const char *glyph(const struct fish *f, int right) {
    if (f->state == 'Z') return right ? "><(((x>" : "<x)))><";
    if (f->age < 8) return f->age < 4 ? "o" : "(@)";
    if (f->rss < 64) return right ? "><>" : "<><";
    if (f->rss < 2048) return right ? "><(((o>" : "<o)))><";
    return right ? "}<((((((*>" : "<*))))))>{";
}

static void draw(int frame) {
    for (int r = 0; r < rows; r++)
        for (int c = 0; c < cols; c++) scr[r][c] = (struct cell){' ', 250, 0, 0};

    // Water, waves, sea bed, weed.
    for (int l = 0; l < nlanes; l++) {
        unsigned char bg = (l & 1) ? 18 : 17;
        for (int y = 0; y < lane_h; y++) {
            int r = top + l * lane_h + y;
            for (int c = 0; c < cols; c++) put_bg(r, c, bg);
        }
        int r0 = top + l * lane_h;
        for (int c = 0; c < cols; c++)
            if (((c + frame / 3 + l * 7) % 9) == 0) put(r0, c, '~', 39, 0);
        if (lane_h >= 3) {
            int rb = r0 + lane_h - 1;
            for (int c = 0; c < cols; c++) put(rb, c, (c * 7 + l) % 5 ? '_' : '.', 136, 0);
            for (int c = 14 + l * 5 % 11; c < cols; c += 23) {
                int sway = ((frame / 6) + c) & 1;
                put(rb, c, sway ? '(' : ')', 34, 1);
                if (lane_h >= 4) put(rb - 1, c, sway ? ')' : '(', 34, 1);
            }
        }
    }

    // Bubbles.
    for (int i = 0; i < MAXBUB; i++) if (bub[i].used)
        put((int)(bub[i].y + 0.5f), (int)bub[i].x, bub[i].ch, 195, 0);

    // Fish.
    for (int i = 0; i < MAXFISH; i++) {
        struct fish *f = &fish[i];
        if (!f->used) continue;
        int right = f->vx >= 0;
        const char *g = glyph(f, right);
        int len = strlen(g), r = (int)(f->ay + 0.5f), x = (int)f->x;
        unsigned char fg = f->color;
        if (f->state == 'Z') fg = 244;
        else if (f->state == 'T') fg = 240;
        else if (f->pid == self_pid) fg = 231;
        int bold = f->delta > 0;
        puts_at(r, x, g, fg, bold);
        if (f->state == 'S' && f->delta == 0 && f->age >= 8 && ((frame / 8 + f->pid) % 3) == 0)
            put(r - 1, right ? x + len : x - 1, 'z', 252, 0);
        if (f->state == 'T') puts_at(r - 1, x + len / 2, "||", 245, 1);
        if (show_names) {
            char label[24];
            snprintf(label, sizeof label, "%s%s", f->name, f->pid == self_pid ? "(yo)" : "");
            int ll = strlen(label);
            int lx = right ? x - ll - 1 : x + len + 1;
            puts_at(r, lx, label, f->delta > 0 ? 253 : 245, 0);
        }
    }

    // Lane labels, drawn last so they stay readable.
    for (int l = 0; l < nlanes; l++) {
        char lab[24];
        int c = l, pct = cpu_pct[c];
        if (l == nlanes - 1 && ncpu > nlanes) snprintf(lab, sizeof lab, "cpu%d+ ", c);
        else snprintf(lab, sizeof lab, "cpu%-2d %3d%% ", c, pct);
        int r = top + l * lane_h;
        unsigned char fg = pct > 66 ? 203 : pct > 20 ? 221 : 121;
        puts_at(r, 0, lab, fg, 1);
        for (int k = 0; k < 6; k++) put(r, strlen(lab) + k, k < (pct + 16) / 17 ? '#' : '.', fg, 0);
    }

    int nproc = 0, nrun = 0;
    for (int i = 0; i < MAXFISH; i++) if (fish[i].used) { nproc++; if (fish[i].delta > 0) nrun++; }
    char hdr[256];
    snprintf(hdr, sizeof hdr,
             " acuario del scheduler | %d CPUs | %d peces, %d nadando | nacimientos %ld  muertes %ld  migraciones %ld%s",
             ncpu, nproc, nrun, births, deaths, migrations, paused ? "  [PAUSA]" : "");
    for (int c = 0; c < cols; c++) put_bg(0, c, 24);
    puts_at(0, 0, hdr, 231, 1);
    const char *help = " q salir   f alimentar   F cardumen   n nombres   espacio pausa";
    for (int c = 0; c < cols; c++) put_bg(rows - 1, c, 236);
    puts_at(rows - 1, 0, help, 250, 0);
}

static void flush_screen(void) {
    static char out[MAXROWS * MAXCOLS * 24];
    int n = 0, fg = -1, bg = -1, bold = -1;
    n += sprintf(out + n, "\x1b[H");
    for (int r = 0; r < rows; r++) {
        n += sprintf(out + n, "\x1b[%d;1H", r + 1);
        // The bottom-right cell is never written: on a console without
        // ?7l it would wrap and scroll the whole tank up a line.
        for (int c = 0; c < (r == rows - 1 ? cols - 1 : cols); c++) {
            struct cell *s = &scr[r][c];
            if (s->bold != bold) { n += sprintf(out + n, s->bold ? "\x1b[1m" : "\x1b[22m"); bold = s->bold; }
            if (s->fg != fg) { n += sprintf(out + n, "\x1b[38;5;%dm", s->fg); fg = s->fg; }
            if (s->bg != bg) { n += sprintf(out + n, "\x1b[48;5;%dm", s->bg); bg = s->bg; }
            out[n++] = s->ch;
        }
    }
    for (int off = 0; off < n; ) {
        int w = write(1, out + off, n - off);
        if (w <= 0) break;
        off += w;
    }
}

// ---------------------------------------------------------------- motion

static void step(void) {
    for (int i = 0; i < MAXFISH; i++) {
        struct fish *f = &fish[i];
        if (!f->used) continue;
        f->age++;
        float target = lane_row(f->cpu, f->fy);
        f->ay += (target - f->ay) * 0.35f;
        if (f->state == 'Z') { // belly-up: drift towards the surface of its lane
            if (f->fy > 0 && (f->age & 7) == 0) f->fy--;
            continue;
        }
        if (f->state == 'T' || f->age < 8) continue;
        float speed = f->delta > 0 ? 0.35f + (f->delta > 20 ? 20 : f->delta) * 0.08f : 0.06f;
        f->vx = f->vx >= 0 ? speed : -speed;
        f->x += f->vx;
        int len = strlen(glyph(f, 1));
        if (f->x < 0) { f->x = 0; f->vx = speed; }
        if (f->x > cols - len - 1) { f->x = cols - len - 1; f->vx = -speed; }
        if (f->delta == 0 && (rnd() % 200) == 0) f->vx = -f->vx;
        if (f->delta > 0 && (rnd() % 6) == 0)
            add_bubbles(f->vx > 0 ? f->x + len : f->x - 1, f->ay - 1, 1, 3);
    }
    for (int i = 0; i < MAXBUB; i++) if (bub[i].used) {
        bub[i].y -= bub[i].vy;
        bub[i].x += (frand() - 0.5f) * 0.6f;
        if (--bub[i].life <= 0 || bub[i].y < top) bub[i].used = 0;
    }
}

// ---------------------------------------------------------------- food

// mlibc has no prctl(); 157 is Linux's prctl, 15 its PR_SET_NAME.
static void set_name(const char *name) {
    long ret;
    asm volatile ("syscall" : "=a"(ret) : "a"(157L), "D"(15L), "S"(name) : "rcx", "r11", "memory");
    (void)ret;
}

static void hungry_child(void) {
    set_name("comida");
    struct timespec t0, t;
    clock_gettime(CLOCK_MONOTONIC, &t0);
    volatile unsigned long spin = 0;
    for (;;) {
        for (int i = 0; i < 100000; i++) spin += i;
        clock_gettime(CLOCK_MONOTONIC, &t);
        if (t.tv_sec - t0.tv_sec >= 3 + (int)(t0.tv_nsec % 3)) _exit(0);
    }
}

static void feed(int n) {
    for (int i = 0; i < n; i++) {
        pid_t p = fork();
        if (p == 0) hungry_child();
    }
}

// ---------------------------------------------------------------- main

static void layout(void) {
    struct winsize ws;
    if (ioctl(1, TIOCGWINSZ, &ws) == 0 && ws.ws_row > 4 && ws.ws_col > 20) { rows = ws.ws_row; cols = ws.ws_col; }
    if (rows > MAXROWS) rows = MAXROWS;
    if (cols > MAXCOLS) cols = MAXCOLS;
    int tank = rows - 2;
    nlanes = ncpu < tank ? ncpu : tank;
    lane_h = tank / nlanes;
    if (lane_h < 1) lane_h = 1;
}

int main(int argc, char **argv) {
    int frames_limit = argc > 1 ? atoi(argv[1]) : 0; // for unattended runs
    self_pid = getpid();
    rng ^= (unsigned)self_pid * 2654435761u;
    signal(SIGINT, on_signal);
    signal(SIGTERM, on_signal);

    int tty = tcgetattr(0, &saved_tio) == 0;
    if (tty) {
        struct termios raw = saved_tio;
        raw.c_lflag &= ~(ICANON | ECHO);
        raw.c_cc[VMIN] = 0; raw.c_cc[VTIME] = 0;
        tcsetattr(0, TCSANOW, &raw);
    }
    printf("\x1b[?1049h\x1b[?25l\x1b[?7l\x1b[2J");
    fflush(stdout);

    sample_cpus();
    layout();
    sample_procs(0);

    for (int frame = 0, gen = 1; !quit; frame++) {
        if (frames_limit && frame >= frames_limit) break;
        if (!paused && frame % SAMPLE_EVERY == 0) {
            while (waitpid(-1, NULL, WNOHANG) > 0) {}
            sample_cpus();
            layout();
            sample_procs(gen++);
        }
        if (!paused) step();
        draw(frame);
        flush_screen();

        struct pollfd pfd = {0, POLLIN, 0};
        if (poll(&pfd, 1, FRAME_MS) > 0 && (pfd.revents & POLLIN)) {
            char k[16];
            int n = read(0, k, sizeof k);
            for (int i = 0; i < n; i++) switch (k[i]) {
                case 'q': case 'Q': quit = 1; break;
                case 'f': feed(1); break;
                case 'F': feed(ncpu); break;
                case 'n': show_names = !show_names; break;
                case ' ': paused = !paused; break;
            }
        }
    }

    printf("\x1b[0m\x1b[?7h\x1b[?25h\x1b[?1049l");
    printf("acuario: %ld nacimientos, %ld muertes, %ld migraciones\n", births, deaths, migrations);
    fflush(stdout);
    if (tty) tcsetattr(0, TCSANOW, &saved_tio);
    while (waitpid(-1, NULL, WNOHANG) > 0) {}
    return 0;
}
