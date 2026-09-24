// fire — the PSX DOOM fire effect, drawn through /dev/fb's FBIO_BLIT.
//
// A demo that doubles as a bare-metal check of the framebuffer path:
// every frame is one full FBIO_BLIT (scaled by the kernel to the whole
// screen), and the frame rate is printed on exit. Any key quits;
// EVIOCGRAB keeps that key out of the shell afterwards, same as doom.

#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/ioctl.h>
#include <time.h>
#include <unistd.h>

#define FBIO_BLIT 0x46420001UL
#define EVIOCGRAB 0x40044590UL
#define EV_KEY 1

#define W 320
#define H 200

struct fb_blit_args {
    unsigned long ptr;
    unsigned int width;
    unsigned int height;
};

struct input_event {
    long tv_sec;
    long tv_usec;
    unsigned short type;
    unsigned short code;
    int value;
};

// The 37-entry palette from the original effect, black -> white.
static const uint32_t PALETTE[37] = {
    0x070707, 0x1F0707, 0x2F0F07, 0x470F07, 0x571707, 0x671F07, 0x771F07,
    0x8F2707, 0x9F2F07, 0xAF3F07, 0xBF4707, 0xC74707, 0xDF4F07, 0xDF5707,
    0xDF5707, 0xD75F07, 0xD75F07, 0xD7670F, 0xCF6F0F, 0xCF770F, 0xCF7F0F,
    0xCF8717, 0xC78717, 0xC78F17, 0xC7971F, 0xBF9F1F, 0xBF9F1F, 0xBFA727,
    0xBFA727, 0xBFAF2F, 0xB7AF2F, 0xB7B72F, 0xB7B737, 0xCFCF6F, 0xDFDF9F,
    0xEFEFC7, 0xFFFFFF,
};

static uint8_t heat[W * H];
static uint32_t pixels[W * H];
static uint32_t rng = 0x12345678;

static uint32_t next_rand(void) {
    rng ^= rng << 13;
    rng ^= rng >> 17;
    rng ^= rng << 5;
    return rng;
}

static double now_s(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec + ts.tv_nsec / 1e9;
}

// Each cell takes its heat from the one below, cooled by 0 or 1 and
// blown sideways by up to one cell to the left.
static void spread(void) {
    for (int y = 1; y < H; y++) {
        for (int x = 0; x < W; x++) {
            int src = y * W + x;
            uint32_t r = next_rand() & 3;
            int h = heat[src] - (int)(r & 1);
            int dst = src - W - (int)r + 1;
            if (dst < 0) dst = 0;
            heat[dst] = h < 0 ? 0 : (uint8_t)h;
        }
    }
}

int main(void) {
    int fb = open("/dev/fb", O_WRONLY);
    if (fb < 0) {
        printf("fire: cannot open /dev/fb\n");
        return 1;
    }
    int kbd = open("/dev/input/event0", O_RDONLY);
    if (kbd >= 0) {
        ioctl(kbd, EVIOCGRAB, 1);
        // Drop the backlog, including the Enter that launched us.
        struct input_event ev;
        while (read(kbd, &ev, sizeof(ev)) == (long)sizeof(ev)) { }
    }

    for (int x = 0; x < W; x++) heat[(H - 1) * W + x] = 36;

    struct fb_blit_args args = { (unsigned long)pixels, W, H };
    unsigned long frames = 0;
    double start = now_s();
    double blit_total = 0;
    int running = 1;

    while (running) {
        spread();
        for (int i = 0; i < W * H; i++) pixels[i] = PALETTE[heat[i]];

        double t0 = now_s();
        ioctl(fb, FBIO_BLIT, &args);
        blit_total += now_s() - t0;
        frames++;

        struct input_event ev;
        while (kbd >= 0 && read(kbd, &ev, sizeof(ev)) == (long)sizeof(ev)) {
            if (ev.type == EV_KEY && ev.value == 1) running = 0;
        }

        // Cap at ~60 fps so the flames move at the speed they were tuned for.
        double elapsed = now_s() - t0;
        if (elapsed < 1.0 / 60) {
            struct timespec ts = { 0, (long)((1.0 / 60 - elapsed) * 1e9) };
            nanosleep(&ts, NULL);
        }
    }

    double total = now_s() - start;
    if (kbd >= 0) ioctl(kbd, EVIOCGRAB, 0);
    printf("fire: %lu frames in %.2f s = %.1f fps, blit %.2f ms/frame\n",
           frames, total, frames / total, blit_total * 1000 / frames);
    return 0;
}
