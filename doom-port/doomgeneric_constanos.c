// doom-port/doomgeneric_constanos.c
//
// doomgeneric platform port for this kernel (see doomgeneric/doomgeneric.h
// for the interface every port implements). Our code, not upstream —
// lives outside the doomgeneric/ submodule and outside userspace/c/ (that
// dir's build.rs loop is one-.c-file-per-program; this is compiled as part
// of a whole-engine multi-file build by scripts/build-doom.sh instead, the
// same "bespoke build script" shape as scripts/build-busybox.sh).
//
// Three kernel-specific primitives back this port:
//   - /dev/fb's FBIO_BLIT ioctl (kernel/src/process/syscall.rs) — hands the
//     kernel our own offscreen pixel buffer once a frame; it scales/blits
//     it into the real framebuffer itself (see Framebuffer::blit_scaled).
//   - /dev/input/event0 (kernel/src/drivers/dev_input_event.rs) —
//     non-blocking real press/release events, sourced from the PS/2 IRQ's
//     raw scancode decode (kernel/src/keyboard.rs), instead of /dev/kbd's
//     char/ANSI-escape stream (no key-up events there). Wire-compatible
//     with real Linux evdev: each read() returns one real
//     `struct input_event` (EV_KEY + a real linux/input-event-codes.h
//     KEY_* code + press/release value, immediately followed by an
//     EV_SYN/SYN_REPORT), the exact protocol an unmodified Linux evdev
//     client would expect — this port just happens to be the one reading
//     it. An earlier version of this port used a bespoke [scancode,
//     pressed] 2-byte format over /dev/kbdraw instead; superseded once the
//     driver itself started speaking real evdev.
//   - /dev/input/event1 (kernel/src/drivers/dev_mouse_event.rs) — same
//     real evdev wire format as event0 above, sourced from the PS/2
//     auxiliary device (kernel/src/mouse.rs, IRQ12): EV_REL (REL_X/REL_Y)
//     for relative motion, EV_KEY (BTN_LEFT/RIGHT/MIDDLE) for buttons.
//     Drives mouse-look via D_PostEvent(ev_mouse) — see DG_DrawFrame.
//   - /mnt/freedoom1.wad — the IWAD itself, read straight off the ext2
//     image (fs::ext2, read-only, seeded from disk-image-root/ at build
//     time). An earlier version of this port routed the WAD through a
//     kernel-embedded /dev/freedoom1.wad device instead, worked around
//     what looked like transient ATA read corruption under DOOM's access
//     pattern (fopen + SEEK_END size probe + scattered lump reads) — the
//     real cause turned out to be an mlibc sysdeps ABI bug (SEEK_SET was
//     wired to the wrong numeric value, so any SEEK_END size probe failed
//     silently and made the file "go empty"), long since fixed — see the
//     SEEK_SET bug note in mlibc-port-and-kernel-bugs. With that fixed,
//     ext2 works fine and the embedded-device workaround (and its
//     kernel-image size cost) is gone.
//   - /dev/dsp (kernel/src/drivers/dev_dsp.rs, backed by the AC97 PCI
//     driver kernel/src/ac97.rs) — fixed-format (48000 Hz stereo s16le)
//     PCM output for sound effects. See doomgeneric_sound_constanos.c
//     (this port's sound_module_t, a separate file — self-contained
//     mixing/resampling subsystem, not folded in here). Sound effects
//     only, no music: this doomgeneric fork has no MIDI/OPL synthesis
//     backend at all, unrelated to the audio driver work.
//
// Audio note: FEATURE_SOUND is defined for this build (see
// scripts/build-doom.sh) — see doomgeneric_sound_constanos.c above.

#include "doomkeys.h"
#include "doomgeneric.h"
#include "d_event.h" // event_t, ev_mouse, D_PostEvent — mouse-look

#include <fcntl.h>
#include <unistd.h>
#include <sys/ioctl.h>
#include <time.h>
#include <stdint.h>
#include <string.h>

// Custom, this-kernel-only ioctl request code — see sys_ioctl's FBIO_BLIT
// (not a real Linux fbdev ioctl; real fbdev exposes the framebuffer via
// mmap, which this kernel doesn't support for device memory).
#define FBIO_BLIT 0x46420001UL

struct fb_blit_args {
    unsigned long ptr;
    unsigned int width;
    unsigned int height;
};

// This kernel-specific syscall (above the Linux syscall range, see
// CLAUDE.md's syscall table) has no mlibc wrapper, so call it directly —
// same raw-syscall convention userspace/c/kdebug.c uses for its own
// kernel-specific syscall.
#define SYS_UPTIME_MS 400

static long raw_syscall0(long nr)
{
    long ret;
    register long r10 asm("r10") = 0;
    register long r8  asm("r8")  = 0;
    asm volatile ("syscall"
            : "=a"(ret)
            : "a"(nr), "D"(0), "S"(0), "d"(0), "r"(r10), "r"(r8)
            : "rcx", "r11", "memory");
    return ret;
}

static int s_fbFd = -1;
static int s_kbdFd = -1;
static int s_mouseFd = -1;
static int s_mouseButtons = 0; // persistent held-button bitmask (bit0=L,1=R,2=M)

#define KEYQUEUE_SIZE 16
static unsigned short s_KeyQueue[KEYQUEUE_SIZE];
static unsigned int s_KeyQueueWriteIndex = 0;
static unsigned int s_KeyQueueReadIndex = 0;

// Real Linux evdev protocol constants (linux/input-event-codes.h,
// linux/input.h) — defined locally since the sysroot doesn't ship those
// headers, not because the values are made up; these are the actual
// upstream numbers, which is what makes /dev/input/event0 a real evdev
// device rather than a lookalike.
#define EV_SYN 0x00
#define EV_KEY 0x01
#define EV_REL 0x02
#define SYN_REPORT 0
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

// Unshifted base-key table indexed by Linux KEY_* code for the "base"
// (non-extended) keyboard block. No translation needed here: Linux's
// KEY_* numbering for this block was carried over directly from the
// original PC/AT Set-1 scancodes (KEY_ESC=1, KEY_1=2, ... KEY_SPACE=57),
// so this table is literally the same shape as
// kernel/src/keyboard.rs::scancode_to_base_char minus the
// shift/capslock/ctrl transforms — i_input.c derives the shifted "typed
// char" itself from raw key id + its own shift tracking, once we forward
// KEY_RSHIFT press/release events (see convertToDoomKey below), so this
// table only needs to name each *key*, not every shifted variant.
static const unsigned char s_baseKeys[58] = {
    /*0*/  0,
    /*1*/  KEY_ESCAPE,
    /*2*/  '1','2','3','4','5','6','7','8','9','0','-','=',
    /*14*/ KEY_BACKSPACE,
    /*15*/ KEY_TAB,
    /*16*/ 'q','w','e','r','t','y','u','i','o','p','[',']',
    /*28*/ KEY_ENTER,
    /*29*/ 0, // KEY_LEFTCTRL — handled in convertToDoomKey
    /*30*/ 'a','s','d','f','g','h','j','k','l',';','\'','`',
    /*42*/ 0, // KEY_LEFTSHIFT — handled in convertToDoomKey
    /*43*/ '\\',
    /*44*/ 'z','x','c','v','b','n','m',',','.','/',
    /*54*/ 0, // KEY_RIGHTSHIFT — handled in convertToDoomKey
    /*55*/ 0, // KEY_KPASTERISK — unused
    /*56*/ 0, // KEY_LEFTALT — handled in convertToDoomKey
    /*57*/ ' ',
};

static unsigned char convertToDoomKey(unsigned short code)
{
    switch (code) {
        case 29: case 97:  return KEY_FIRE;    // KEY_LEFTCTRL / KEY_RIGHTCTRL
        case 42: case 54:  return KEY_RSHIFT;  // KEY_LEFTSHIFT / KEY_RIGHTSHIFT
        case 56: case 100: return KEY_RALT;    // KEY_LEFTALT / KEY_RIGHTALT
        case 103: return KEY_UPARROW;
        case 108: return KEY_DOWNARROW;
        case 105: return KEY_LEFTARROW;
        case 106: return KEY_RIGHTARROW;
        case 102: return KEY_HOME;
        case 107: return KEY_END;
        case 104: return KEY_PGUP;
        case 109: return KEY_PGDN;
        case 110: return KEY_INS;
        case 111: return KEY_DEL;
        case 57:  return KEY_USE; // KEY_SPACE
        default:
            if (code < sizeof(s_baseKeys)) {
                return s_baseKeys[code];
            }
            return 0;
    }
}

static void addKeyToQueue(int pressed, unsigned short code)
{
    unsigned char key = convertToDoomKey(code);
    if (key == 0) {
        return; // unmapped code (F-keys, numlock, ...) — ignore
    }
    unsigned short keyData = ((unsigned short)(pressed ? 1 : 0) << 8) | key;
    s_KeyQueue[s_KeyQueueWriteIndex] = keyData;
    s_KeyQueueWriteIndex = (s_KeyQueueWriteIndex + 1) % KEYQUEUE_SIZE;
}

void DG_Init(void)
{
    s_fbFd = open("/dev/fb", O_WRONLY);
    s_kbdFd = open("/dev/input/event0", O_RDONLY);
    s_mouseFd = open("/dev/input/event1", O_RDONLY);

    // Drain events queued before we started: the kernel's raw-event ring
    // buffer fills from every keypress since boot and nothing else reads
    // it, so the shell commands that launched us (and anything typed
    // earlier) are still queued — without this, DOOM replays that backlog
    // into the title screen (observed: stray Enters walking the menu into
    // episode select on their own).
    struct input_event ev;
    while (s_kbdFd >= 0 && read(s_kbdFd, &ev, sizeof(ev)) == (long)sizeof(ev)) { }
    while (s_mouseFd >= 0 && read(s_mouseFd, &ev, sizeof(ev)) == (long)sizeof(ev)) { }
}

void DG_DrawFrame(void)
{
    struct input_event ev;
    while (s_kbdFd >= 0 && read(s_kbdFd, &ev, sizeof(ev)) == (long)sizeof(ev)) {
        if (ev.type == EV_KEY) {
            addKeyToQueue(ev.value != 0, ev.code);
        }
        // EV_SYN/SYN_REPORT and anything else this device doesn't emit:
        // nothing to do, just consume it.
    }

    // Accumulate every mouse packet since the last frame into one
    // ev_mouse event — matches how a real port (e.g. SDL relative mouse
    // mode) coalesces motion between ticks instead of posting one event
    // per PS/2 packet. data2/data3 are raw evdev REL_X/REL_Y deltas,
    // unnegated: PS/2's own sign convention (X+ = right, Y+ = up/away
    // from the user) already matches what g_game.c's mouse handling
    // expects (angleturn -= mousex*0x8 turns right on X+; forward +=
    // mousey advances on Y+) — this is the same raw convention the
    // original DOS mouse driver reported through, which is what that
    // formula was written against.
    if (s_mouseFd >= 0) {
        int mdx = 0, mdy = 0;
        int haveMouseEvent = 0;
        while (read(s_mouseFd, &ev, sizeof(ev)) == (long)sizeof(ev)) {
            haveMouseEvent = 1;
            if (ev.type == EV_REL) {
                if (ev.code == REL_X) mdx += ev.value;
                else if (ev.code == REL_Y) mdy += ev.value;
            } else if (ev.type == EV_KEY) {
                // Update persistent held-button state (s_mouseButtons),
                // not a per-frame-local one — a button held across
                // several frames with no new transition must still read
                // as held every frame, not just the frame it was pressed.
                int bit = (ev.code == BTN_LEFT) ? 1
                        : (ev.code == BTN_RIGHT) ? 2
                        : (ev.code == BTN_MIDDLE) ? 4 : 0;
                if (bit) {
                    if (ev.value) s_mouseButtons |= bit;
                    else s_mouseButtons &= ~bit;
                }
            }
            // EV_SYN/SYN_REPORT: nothing to do, just consumed by the loop.
        }
        if (haveMouseEvent) {
            event_t doomEv;
            doomEv.type = ev_mouse;
            doomEv.data1 = s_mouseButtons;
            doomEv.data2 = mdx;
            doomEv.data3 = mdy;
            doomEv.data4 = 0;
            D_PostEvent(&doomEv);
        }
    }

    if (s_fbFd >= 0) {
        struct fb_blit_args args;
        args.ptr = (unsigned long)DG_ScreenBuffer;
        args.width = DOOMGENERIC_RESX;
        args.height = DOOMGENERIC_RESY;
        ioctl(s_fbFd, FBIO_BLIT, &args);
    }
}

void DG_SleepMs(uint32_t ms)
{
    struct timespec ts;
    ts.tv_sec = ms / 1000;
    ts.tv_nsec = (long)(ms % 1000) * 1000000L;
    nanosleep(&ts, NULL);
}

uint32_t DG_GetTicksMs(void)
{
    return (uint32_t)raw_syscall0(SYS_UPTIME_MS);
}

int DG_GetKey(int* pressed, unsigned char* doomKey)
{
    if (s_KeyQueueReadIndex == s_KeyQueueWriteIndex) {
        return 0; // queue empty
    }
    unsigned short keyData = s_KeyQueue[s_KeyQueueReadIndex];
    s_KeyQueueReadIndex = (s_KeyQueueReadIndex + 1) % KEYQUEUE_SIZE;
    *pressed = keyData >> 8;
    *doomKey = keyData & 0xFF;
    return 1;
}

void DG_SetWindowTitle(const char *title)
{
    (void)title; // no window/console title to set
}

int main(int argc, char **argv)
{
    static char *fixedArgv[3];
    if (argc <= 1) {
        fixedArgv[0] = argv[0];
        fixedArgv[1] = "-iwad";
        fixedArgv[2] = "/mnt/freedoom1.wad"; // ext2-served IWAD
        argv = fixedArgv;
        argc = 3;
    }

    doomgeneric_Create(argc, argv);

    for (;;) {
        doomgeneric_Tick();
    }

    return 0;
}
