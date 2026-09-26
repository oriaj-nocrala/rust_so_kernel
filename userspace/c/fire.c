// fire — the PSX DOOM fire effect.
//
// A demo that doubles as a check of the picture path: on the console every
// frame is one full FBIO_BLIT (scaled by the kernel to the whole screen);
// under the compositor ($GUI_DISPLAY) it is a window — both through
// constanos_gfx.h. The frame rate is printed on exit. Any key (or click)
// quits; on the console EVIOCGRAB keeps that key out of the shell
// afterwards, same as doom.

#include <stdint.h>
#include <stdio.h>
#include <time.h>

#include "constanos_gfx.h"

#define W 320
#define H 200

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
    if (gfx_open("fire", W, H, 0) < 0) {
        printf("fire: nothing to draw on (no /dev/fb, no compositor)\n");
        return 1;
    }

    for (int x = 0; x < W; x++) heat[(H - 1) * W + x] = 36;

    unsigned long frames = 0;
    double start = now_s();
    double blit_total = 0;
    int running = 1;

    while (running) {
        spread();
        for (int i = 0; i < W * H; i++) pixels[i] = PALETTE[heat[i]];

        double t0 = now_s();
        gfx_present(pixels);
        blit_total += now_s() - t0;
        frames++;

        struct gfx_event ev;
        while (gfx_next_event(&ev)) {
            if (ev.type == GFX_EV_KEY && ev.value == 1) running = 0;
        }

        // Cap at ~60 fps so the flames move at the speed they were tuned for.
        double elapsed = now_s() - t0;
        if (elapsed < 1.0 / 60) {
            struct timespec ts = { 0, (long)((1.0 / 60 - elapsed) * 1e9) };
            nanosleep(&ts, NULL);
        }
    }

    double total = now_s() - start;
    gfx_close();
    printf("fire: %lu frames in %.2f s = %.1f fps, blit %.2f ms/frame\n",
           frames, total, frames / total, blit_total * 1000 / frames);
    return 0;
}
