// quake-port/quakegeneric_constanos.c
//
// quakegeneric platform port for this kernel (see quakegeneric/source/
// quakegeneric.h for the interface every port implements). Our code, not
// upstream — lives outside the quakegeneric/ submodule, mirroring
// doom-port/doomgeneric_constanos.c's shape almost exactly (same fb/input
// devices, same evdev wire format). Built by scripts/build-quake.sh (a
// whole-engine multi-file build, not the one-.c-per-program convention in
// userspace/c/'s build.rs loop), same idea as scripts/build-doom.sh.
//
// Differences from the DOOM port worth calling out:
//   - quakegeneric hands QG_DrawFrame an 8bpp *paletted* 320x240 buffer,
//     not ready-to-blit RGB — QG_SetPalette supplies the 256-entry RGB
//     palette separately, so this port has to do the index->RGB
//     conversion itself before FBIO_BLIT (doomgeneric already hands back
//     a real RGB buffer, no conversion needed there).
//   - QG_GetKey/QG_GetMouseMove are pull-based (the engine calls them from
//     inside its own frame processing, see quakegeneric's in_null.c),
//     unlike doomgeneric's push-one-event-into-D_PostEvent model — so
//     this port drains the kernel's evdev fds into small queues/counters
//     once per outer-loop iteration, and the Get* functions just pop from
//     them.
//   - Real quakegeneric.h has no per-frame draw callback of its own; the
//     platform's own main() drives everything by calling QG_Tick(dt) in
//     a loop after one QG_Create(argc, argv) — dt is real elapsed seconds
//     (a double), computed here via clock_gettime(CLOCK_MONOTONIC, ...)
//     (this kernel's real RTC-backed monotonic clock, wired the same
//     session as the FPU/SSE work Quake's floating-point-heavy renderer
//     actually exercises — see kernel/src/process/fpu.rs and
//     kernel/src/rtc.rs).
//
// Same three kernel-specific primitives the DOOM port already uses:
//   - /dev/fb's FBIO_BLIT ioctl — hands the kernel a 0x00RRGGBB buffer;
//     it scales/blits into the real framebuffer (Framebuffer::blit_scaled).
//   - /dev/input/event0 (keyboard) / /dev/input/event1 (PS/2 mouse) — real
//     Linux evdev wire format, see doom-port/doomgeneric_constanos.c's own
//     header comment for the full rationale (kernel/src/drivers/
//     dev_input_event.rs, dev_mouse_event.rs). Same "drain the backlog at
//     startup" requirement (the ring buffer fills from every keypress
//     since boot).
//   - id1/pak0.pak — read straight off ext2 (/mnt, seeded from
//     disk-image-root/id1/pak0.pak by scripts/fetch-quake-shareware.sh),
//     same real-filesystem read path (fopen/fread/fseek) the DOOM port
//     already validated works over ext2 once the SEEK_SET ABI bug was
//     fixed (see mlibc-port-and-kernel-bugs memory / CLAUDE.md).
//
// Sound: this v1 links upstream's own snd_null.c (silent) to bound scope
// — real /dev/dsp output (reusing kernel/src/ac97.rs, same as DOOM's
// sound port) is a natural follow-up, not required for "compile and run."

#include "quakekeys.h"
#include "quakegeneric.h"

#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <time.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>

// Same custom, this-kernel-only ioctl request code as the DOOM port (see
// sys_ioctl's FBIO_BLIT handling, kernel/src/process/syscall/fs.rs).
#define FBIO_BLIT 0x46420001UL

struct fb_blit_args {
    unsigned long ptr;
    unsigned int width;
    unsigned int height;
};

static int s_fbFd = -1;
static int s_kbdFd = -1;
static int s_mouseFd = -1;

static unsigned char s_palette[768];
static uint32_t *s_rgbBuffer;

// Real Linux evdev protocol constants — same subset doom-port's own file
// defines locally (sysroot doesn't ship linux/input-event-codes.h), see
// that file's comment for why these are the real upstream numbers.
#define EV_KEY 0x01
#define EV_REL 0x02
#define REL_X 0x00
#define REL_Y 0x01
#define BTN_LEFT   0x110
#define BTN_RIGHT  0x111
#define BTN_MIDDLE 0x112

// Wire-compatible with the real Linux `struct input_event` on x86_64 —
// see kernel/src/drivers/dev_input_event.rs's matching Rust definition.
struct input_event {
    long tv_sec;
    long tv_usec;
    unsigned short type;
    unsigned short code;
    int value;
};

// Unshifted base-key table indexed by evdev/Set-1 scancode, same shape
// (and same source scancodes) as doom-port/doomgeneric_constanos.c's
// s_baseKeys — Quake's own convention is simpler than Doom's: ordinary
// keys are just their lowercased ASCII value (quakekeys.h), so this table
// only needs K_* special-casing for the handful of keys that aren't
// printable ASCII (backspace/tab/enter/space/ctrl/shift/alt).
static const int s_baseKeys[58] = {
    /*0*/  0,
    /*1*/  K_ESCAPE,
    /*2*/  '1','2','3','4','5','6','7','8','9','0','-','=',
    /*14*/ K_BACKSPACE,
    /*15*/ K_TAB,
    /*16*/ 'q','w','e','r','t','y','u','i','o','p','[',']',
    /*28*/ K_ENTER,
    /*29*/ K_CTRL, // KEY_LEFTCTRL
    /*30*/ 'a','s','d','f','g','h','j','k','l',';','\'','`',
    /*42*/ K_SHIFT, // KEY_LEFTSHIFT
    /*43*/ '\\',
    /*44*/ 'z','x','c','v','b','n','m',',','.','/',
    /*54*/ K_SHIFT, // KEY_RIGHTSHIFT
    /*55*/ 0, // KEY_KPASTERISK — unused
    /*56*/ K_ALT, // KEY_LEFTALT
    /*57*/ K_SPACE,
};

static int convertToQuakeKey(unsigned short code)
{
    switch (code) {
        case 29: case 97:  return K_CTRL;       // LEFTCTRL / RIGHTCTRL
        case 42: case 54:  return K_SHIFT;      // LEFTSHIFT / RIGHTSHIFT
        case 56: case 100: return K_ALT;        // LEFTALT / RIGHTALT
        case 103: return K_UPARROW;
        case 108: return K_DOWNARROW;
        case 105: return K_LEFTARROW;
        case 106: return K_RIGHTARROW;
        case 102: return K_HOME;
        case 107: return K_END;
        case 104: return K_PGUP;
        case 109: return K_PGDN;
        case 110: return K_INS;
        case 111: return K_DEL;
        default:
            if (code < sizeof(s_baseKeys) / sizeof(s_baseKeys[0])) {
                return s_baseKeys[code];
            }
            return 0;
    }
}

// Pending-key queue: QG_GetKey is pull-based (called repeatedly by the
// engine's IN_Commands until it returns 0), so pump_input() below fills
// this once per outer-loop iteration and QG_GetKey just drains it.
#define KEYQUEUE_SIZE 32
static int s_keyQueueDown[KEYQUEUE_SIZE];
static int s_keyQueueKey[KEYQUEUE_SIZE];
static unsigned int s_keyQueueWrite = 0;
static unsigned int s_keyQueueRead = 0;

static void pushKey(int down, int key)
{
    if (key == 0) {
        return; // unmapped code — ignore, same convention as the DOOM port
    }
    unsigned int next = (s_keyQueueWrite + 1) % KEYQUEUE_SIZE;
    if (next == s_keyQueueRead) {
        return; // queue full — drop rather than overwrite unread events
    }
    s_keyQueueDown[s_keyQueueWrite] = down;
    s_keyQueueKey[s_keyQueueWrite] = key;
    s_keyQueueWrite = next;
}

// Persistent (not per-frame-local) mouse delta accumulator — QG_GetMouseMove
// reads and zeros it, matching quakegeneric_sdl2.c's own reference shape.
static int s_mouseDx = 0, s_mouseDy = 0;

static void pump_input(void)
{
    struct input_event ev;

    while (s_kbdFd >= 0 && read(s_kbdFd, &ev, sizeof(ev)) == (long)sizeof(ev)) {
        if (ev.type == EV_KEY) {
            pushKey(ev.value != 0, convertToQuakeKey(ev.code));
        }
    }

    while (s_mouseFd >= 0 && read(s_mouseFd, &ev, sizeof(ev)) == (long)sizeof(ev)) {
        if (ev.type == EV_REL) {
            // PS/2's sign convention (X+ = right, Y+ = up/away from the
            // user) already matches what the DOOM port validated works
            // unnegated against id-software mouse-look math; Quake's own
            // in_null.c::IN_MouseMove expects the same raw convention a
            // real DOS mouse driver reported through.
            if (ev.code == REL_X) s_mouseDx += ev.value;
            else if (ev.code == REL_Y) s_mouseDy += ev.value;
        } else if (ev.type == EV_KEY) {
            int key = (ev.code == BTN_LEFT) ? K_MOUSE1
                    : (ev.code == BTN_RIGHT) ? K_MOUSE2
                    : (ev.code == BTN_MIDDLE) ? K_MOUSE3 : 0;
            if (key) {
                pushKey(ev.value != 0, key);
            }
        }
    }
}

void QG_Init(void)
{
    s_fbFd = open("/dev/fb", O_WRONLY);
    s_kbdFd = open("/dev/input/event0", O_RDONLY);
    s_mouseFd = open("/dev/input/event1", O_RDONLY);

    // Drain events queued before we started — same "ring buffer fills
    // from every keypress since boot" reason as the DOOM port (otherwise
    // whatever launched us, e.g. the shell command itself, replays into
    // the title screen).
    struct input_event ev;
    while (s_kbdFd >= 0 && read(s_kbdFd, &ev, sizeof(ev)) == (long)sizeof(ev)) { }
    while (s_mouseFd >= 0 && read(s_mouseFd, &ev, sizeof(ev)) == (long)sizeof(ev)) { }

    s_rgbBuffer = malloc(QUAKEGENERIC_RES_X * QUAKEGENERIC_RES_Y * sizeof(uint32_t));
}

void QG_Quit(void)
{
    if (s_fbFd >= 0) close(s_fbFd);
    if (s_kbdFd >= 0) close(s_kbdFd);
    if (s_mouseFd >= 0) close(s_mouseFd);
    free(s_rgbBuffer);
}

void QG_SetPalette(unsigned char palette[768])
{
    memcpy(s_palette, palette, 768);
}

void QG_DrawFrame(void *pixels)
{
    if (!s_rgbBuffer) {
        return;
    }

    const unsigned char *idx = (const unsigned char *)pixels;
    for (int i = 0; i < QUAKEGENERIC_RES_X * QUAKEGENERIC_RES_Y; i++) {
        const unsigned char *rgb = &s_palette[(unsigned int)idx[i] * 3];
        s_rgbBuffer[i] = ((uint32_t)rgb[0] << 16) | ((uint32_t)rgb[1] << 8) | (uint32_t)rgb[2];
    }

    if (s_fbFd >= 0) {
        struct fb_blit_args args;
        args.ptr = (unsigned long)s_rgbBuffer;
        args.width = QUAKEGENERIC_RES_X;
        args.height = QUAKEGENERIC_RES_Y;
        ioctl(s_fbFd, FBIO_BLIT, &args);
    }
}

int QG_GetKey(int *down, int *key)
{
    if (s_keyQueueRead == s_keyQueueWrite) {
        return 0; // queue empty
    }
    *down = s_keyQueueDown[s_keyQueueRead];
    *key = s_keyQueueKey[s_keyQueueRead];
    s_keyQueueRead = (s_keyQueueRead + 1) % KEYQUEUE_SIZE;
    return 1;
}

void QG_GetMouseMove(int *x, int *y)
{
    *x = s_mouseDx;
    *y = s_mouseDy;
    s_mouseDx = 0;
    s_mouseDy = 0;
}

void QG_GetJoyAxes(float *axes)
{
    // No joystick support — zero every axis (matches quakegeneric_null.c).
    for (int i = 0; i < QUAKEGENERIC_JOY_MAX_AXES; i++) {
        axes[i] = 0.0f;
    }
}

static double now_seconds(void)
{
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (double)ts.tv_sec + (double)ts.tv_nsec / 1e9;
}

int main(int argc, char **argv)
{
    static char *fixedArgv[3];
    if (argc <= 1) {
        // Default to the ext2-served pak, same "no args = sane default"
        // trick doom-port/doomgeneric_constanos.c uses for -iwad.
        fixedArgv[0] = argv[0];
        fixedArgv[1] = "-basedir";
        fixedArgv[2] = "/mnt";
        argv = fixedArgv;
        argc = 3;
    }

    QG_Create(argc, argv);

    double oldtime = now_seconds() - 0.1;
    for (;;) {
        pump_input();
        double newtime = now_seconds();
        QG_Tick(newtime - oldtime);
        oldtime = newtime;
    }

    return 0;
}
