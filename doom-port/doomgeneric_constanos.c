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
//   - /dev/kbdraw (kernel/src/drivers/dev_kbdraw.rs) — non-blocking real
//     press/release events as [scancode, pressed] byte pairs, sourced from
//     the PS/2 IRQ's raw scancode decode (kernel/src/keyboard.rs), instead
//     of /dev/kbd's char/ANSI-escape stream (no key-up events there).
//   - /dev/freedoom1.wad (kernel/src/drivers/dev_wad.rs) — the IWAD itself,
//     embedded in the kernel image and served as a seekable read-only file.
//     The ext2 route (/mnt/freedoom1.wad) was abandoned: DOOM's access
//     pattern triggered transient ATA read corruption that wedged the
//     channel for the rest of the boot. The device is named after the real
//     file because doomgeneric validates the path string itself — see
//     dev_wad.rs's header comment.
//
// No audio: this kernel has no sound driver. FEATURE_SOUND is left
// undefined for this build, which is enough — i_sound.c already behaves
// as a null sound backend when no sound_module is compiled in.

#include "doomkeys.h"
#include "doomgeneric.h"

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

#define KEYQUEUE_SIZE 16
static unsigned short s_KeyQueue[KEYQUEUE_SIZE];
static unsigned int s_KeyQueueWriteIndex = 0;
static unsigned int s_KeyQueueReadIndex = 0;

// Unshifted base-key table for PC/AT Set-1 non-extended scancodes, same
// layout as kernel/src/keyboard.rs::scancode_to_base_char minus the
// shift/capslock/ctrl transforms — i_input.c derives the shifted "typed
// char" itself from raw key id + its own shift tracking, once we forward
// KEY_RSHIFT press/release events (see convertToDoomKey below), so this
// table only needs to name each *key*, not every shifted variant.
static const unsigned char s_baseKeys[0x3A] = {
    /*0x00*/ 0,
    /*0x01*/ KEY_ESCAPE,
    /*0x02*/ '1','2','3','4','5','6','7','8','9','0','-','=',
    /*0x0e*/ KEY_BACKSPACE,
    /*0x0f*/ KEY_TAB,
    /*0x10*/ 'q','w','e','r','t','y','u','i','o','p','[',']',
    /*0x1c*/ KEY_ENTER,
    /*0x1d*/ 0, // left ctrl — handled in convertToDoomKey
    /*0x1e*/ 'a','s','d','f','g','h','j','k','l',';','\'','`',
    /*0x2a*/ 0, // left shift — handled in convertToDoomKey
    /*0x2b*/ '\\',
    /*0x2c*/ 'z','x','c','v','b','n','m',',','.','/',
    /*0x36*/ 0, // right shift — handled in convertToDoomKey
    /*0x37*/ 0, // keypad '*' — unused
    /*0x38*/ 0, // left alt — handled in convertToDoomKey
    /*0x39*/ ' ',
};

static unsigned char convertToDoomKey(unsigned char scancode)
{
    switch (scancode) {
        case 0x1d: case 0x9d: return KEY_FIRE;    // ctrl, left or E0-right
        case 0x2a: case 0x36: return KEY_RSHIFT;  // shift, left or right
        case 0x38: case 0xb8: return KEY_RALT;    // alt, left or E0-right
        case 0xc8: return KEY_UPARROW;    // ext 0x48
        case 0xd0: return KEY_DOWNARROW;  // ext 0x50
        case 0xcb: return KEY_LEFTARROW;  // ext 0x4B
        case 0xcd: return KEY_RIGHTARROW; // ext 0x4D
        case 0xc7: return KEY_HOME;       // ext 0x47
        case 0xcf: return KEY_END;        // ext 0x4F
        case 0xc9: return KEY_PGUP;       // ext 0x49
        case 0xd1: return KEY_PGDN;       // ext 0x51
        case 0xd2: return KEY_INS;        // ext 0x52
        case 0xd3: return KEY_DEL;        // ext 0x53
        case 0x39: return KEY_USE;        // space
        default:
            if (scancode < sizeof(s_baseKeys)) {
                return s_baseKeys[scancode];
            }
            return 0;
    }
}

static void addKeyToQueue(int pressed, unsigned char scancode)
{
    unsigned char key = convertToDoomKey(scancode);
    if (key == 0) {
        return; // unmapped scancode (F-keys, numlock, ...) — ignore
    }
    unsigned short keyData = ((unsigned short)(pressed ? 1 : 0) << 8) | key;
    s_KeyQueue[s_KeyQueueWriteIndex] = keyData;
    s_KeyQueueWriteIndex = (s_KeyQueueWriteIndex + 1) % KEYQUEUE_SIZE;
}

void DG_Init(void)
{
    s_fbFd = open("/dev/fb", O_WRONLY);
    s_kbdFd = open("/dev/kbdraw", O_RDONLY);

    // Drain keystrokes typed before we started: the kernel's raw-event
    // ring buffer fills from every keypress since boot and nothing else
    // reads it, so the shell commands that launched us (and anything
    // typed earlier) are still queued as press/release pairs — without
    // this, DOOM replays that backlog into the title screen (observed:
    // stray Enters walking the menu into episode select on their own).
    unsigned char ev[2];
    while (s_kbdFd >= 0 && read(s_kbdFd, ev, 2) == 2) { }
}

void DG_DrawFrame(void)
{
    unsigned char ev[2];
    while (s_kbdFd >= 0 && read(s_kbdFd, ev, 2) == 2) {
        addKeyToQueue(ev[1], ev[0]);
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
        fixedArgv[2] = "/dev/freedoom1.wad"; // embedded WAD device — see dev_wad.rs
        argv = fixedArgv;
        argc = 3;
    }

    doomgeneric_Create(argc, argv);

    for (;;) {
        doomgeneric_Tick();
    }

    return 0;
}
