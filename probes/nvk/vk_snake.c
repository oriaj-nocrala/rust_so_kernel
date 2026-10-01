/*
 * snake3d: the snake in real 3D on the GPU (docs/gpu/g5-graphics-stack-plan.md, "the first app on NVK"). Vulkan through the statically linked
 * NVK, like vk_draw.c: one static musl executable (build.py). A perspective camera over an arena of neon walls; the snake is a chain of lit,
 * tapering spheres that glide between cells, with blob shadows, a pulsing food orb with orbiting satellites that lights the floor, additive
 * particle bursts, a depth buffer, and a HUD made of 5x7 glyph quads (the font of draw/src/font.rs, one draw per glyph, the bitmap in the push
 * constants). Every object is one draw: there are no vertex buffers, no descriptors, no textures; snake3d.vert builds the geometry from
 * gl_VertexIndex and the push constants, snake3d.frag lights it.
 *
 * On constanos it presents through VK_KHR_swapchain on a VK_EXT_headless_surface, which on this platform is the screen (G5 layer 3: the
 * WSI in Mesa's wsi_common_headless.c): it renders into the swapchain's images at the display's size, and the WSI copies each one on the GPU
 * into a scanout-layout buffer and points the display at it with no CPU copy, FIFO-paced by the flip. It knows nothing of /dev/nvgpu. The 2D
 * snake (userspace/src/bin/snake.rs) stays the version that needs no GPU.
 *
 * Arrows/WASD steer, P pauses, C switches the camera (overview / chase), Space or Enter starts, Esc or Q quits. The title screen plays itself.
 *   SNAKE3D_AUTOPLAY=1   an autopilot plays (and restarts) for unattended runs
 *   SNAKE3D_SECONDS=<n>  quit after n seconds
 *   SNAKE3D_HEADLESS=1   with no display (QEMU's software device): the surface has no fixed size, render 640x360 and present nothing
 *   SNAKE3D_WINDOW=1     a window of the GPU compositor (vk_comp) instead of the screen: the connection is ours ($GUI_DISPLAY), the keys come over it
 *                        (the window must have the focus), the swapchain is made again at whatever size the compositor gives the window (a resize
 *                        drag, maximize, F11 fullscreen); SNAKE3D_W/H the first size (default: what the compositor suggests)
 * The same source builds for the host (-DSNAKE_HOST, host-snake.sh) against the system's Vulkan: it renders offscreen at a fixed 60 Hz step
 * and dumps PPM frames (SNAKE3D_DUMP=<dir> SNAKE3D_DUMP_AT=<frame,frame,...>), which is how the pictures were checked.
 */
#define VK_NO_PROTOTYPES
#include <vulkan/vulkan.h>

#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>
#include <unistd.h>

#include "snake3d_font.h"
#include "snake3d_spv.h"

#ifdef SNAKE_HOST
extern VKAPI_ATTR PFN_vkVoidFunction VKAPI_CALL vkGetInstanceProcAddr(VkInstance instance, const char *name);
#define GET_INSTANCE_PROC vkGetInstanceProcAddr
struct nvg_scanout_info { uint32_t width, height, pitch_B, format; uint64_t size_B, flags; };
#else
#include <fcntl.h>
#include <sys/ioctl.h>
#include <unwind.h>

#include "constanos_gui_vk.h"

extern PFN_vkVoidFunction vk_icdGetInstanceProcAddr(VkInstance instance, const char *name);
#define GET_INSTANCE_PROC vk_icdGetInstanceProcAddr

static _Unwind_Reason_Code trace_cb(struct _Unwind_Context *c, void *arg) {
   (void)arg;
   printf("VK BT %#lx\n", (unsigned long)_Unwind_GetIP(c));
   return _URC_NO_REASON;
}

void __assert_fail(const char *expr, const char *file, int line, const char *func) {
   printf("VK ASSERT %s (%s: %s: %d)\n", expr, file, func, line);
   _Unwind_Backtrace(trace_cb, NULL);
   _exit(134);
}

void abort(void) {
   _Unwind_Backtrace(trace_cb, NULL);
   _exit(134);
}
#endif

/* ── Small maths ────────────────────────────────────────────────────────────────────────────────────────────────────────────── */

typedef struct { float x, y, z; } vec3;
typedef struct { float m[16]; } mat4;   /* column-major: element (row r, column c) is m[c * 4 + r], as GLSL reads it */

static vec3 v3(float x, float y, float z) { return (vec3){ x, y, z }; }
static vec3 v3add(vec3 a, vec3 b) { return v3(a.x + b.x, a.y + b.y, a.z + b.z); }
static vec3 v3sub(vec3 a, vec3 b) { return v3(a.x - b.x, a.y - b.y, a.z - b.z); }
static vec3 v3mul(vec3 a, float k) { return v3(a.x * k, a.y * k, a.z * k); }
static vec3 v3lerp(vec3 a, vec3 b, float t) { return v3add(a, v3mul(v3sub(b, a), t)); }
static float v3dot(vec3 a, vec3 b) { return a.x * b.x + a.y * b.y + a.z * b.z; }
static vec3 v3cross(vec3 a, vec3 b) { return v3(a.y * b.z - a.z * b.y, a.z * b.x - a.x * b.z, a.x * b.y - a.y * b.x); }
static vec3 v3norm(vec3 a) { float l = sqrtf(v3dot(a, a)); return l > 0 ? v3mul(a, 1.0f / l) : a; }
static float clampf(float x, float lo, float hi) { return x < lo ? lo : x > hi ? hi : x; }
static float lerpf(float a, float b, float t) { return a + (b - a) * t; }

static void m4set(mat4 *m, int r, int c, float v) { m->m[c * 4 + r] = v; }

static mat4 m4mul(const mat4 *a, const mat4 *b) {
   mat4 o;
   for (int c = 0; c < 4; c++)
      for (int r = 0; r < 4; r++) {
         float s = 0;
         for (int k = 0; k < 4; k++) s += a->m[k * 4 + r] * b->m[c * 4 + k];
         o.m[c * 4 + r] = s;
      }
   return o;
}

/* Vulkan clip space: y points down the screen, z runs 0 (near) to 1 (far). The view looks down -z with +y up, so the projection negates y. */
static mat4 m4_persp(float fovy, float aspect, float zn, float zf) {
   mat4 m = { { 0 } };
   float f = 1.0f / tanf(fovy * 0.5f);
   m4set(&m, 0, 0, f / aspect);
   m4set(&m, 1, 1, -f);
   m4set(&m, 2, 2, zf / (zn - zf));
   m4set(&m, 2, 3, zn * zf / (zn - zf));
   m4set(&m, 3, 2, -1.0f);
   return m;
}

static mat4 m4_lookat(vec3 eye, vec3 target, vec3 up) {
   vec3 f = v3norm(v3sub(target, eye));
   vec3 s = v3norm(v3cross(f, up));
   vec3 u = v3cross(s, f);
   mat4 m = { { 0 } };
   m4set(&m, 0, 0, s.x); m4set(&m, 0, 1, s.y); m4set(&m, 0, 2, s.z); m4set(&m, 0, 3, -v3dot(s, eye));
   m4set(&m, 1, 0, u.x); m4set(&m, 1, 1, u.y); m4set(&m, 1, 2, u.z); m4set(&m, 1, 3, -v3dot(u, eye));
   m4set(&m, 2, 0, -f.x); m4set(&m, 2, 1, -f.y); m4set(&m, 2, 2, -f.z); m4set(&m, 2, 3, v3dot(f, eye));
   m4set(&m, 3, 3, 1.0f);
   return m;
}

/* Pixels (y down) to clip space, z passed through: the HUD's matrix. */
static mat4 m4_pixels(float w, float h) {
   mat4 m = { { 0 } };
   m4set(&m, 0, 0, 2.0f / w); m4set(&m, 0, 3, -1.0f);
   m4set(&m, 1, 1, 2.0f / h); m4set(&m, 1, 3, -1.0f);
   m4set(&m, 2, 2, 1.0f);
   m4set(&m, 3, 3, 1.0f);
   return m;
}

static void hsv(float h, float s, float v, float *r, float *g, float *b) {
   h = h - floorf(h / 360.0f) * 360.0f;
   float c = v * s, x = c * (1.0f - fabsf(fmodf(h / 60.0f, 2.0f) - 1.0f)), m = v - c;
   float rr = 0, gg = 0, bb = 0;
   if (h < 60) { rr = c; gg = x; } else if (h < 120) { rr = x; gg = c; } else if (h < 180) { gg = c; bb = x; }
   else if (h < 240) { gg = x; bb = c; } else if (h < 300) { rr = x; bb = c; } else { rr = c; bb = x; }
   *r = rr + m; *g = gg + m; *b = bb + m;
}

static uint32_t rng_state = 2463534242u;
static uint32_t rnd(void) {
   uint32_t x = rng_state;
   x ^= x << 13; x ^= x >> 17; x ^= x << 5;
   return rng_state = x;
}
static float rndf(void) { return (rnd() & 0xffffff) / 16777216.0f; }
static int rnd_range(int lo, int hi) { return lo + (int)(rnd() % (uint32_t)(hi - lo)); }

/* ── The game (the rules of userspace/src/bin/snake.rs) ─────────────────────────────────────────────────────────────────────── */

#define GW 24   /* snake3d.frag's ARENA must match */
#define GH 14

typedef struct { int x, y; } Cell;
enum { UP, DOWN, LEFT, RIGHT };
enum { S_TITLE, S_PLAY, S_PAUSED, S_DYING, S_OVER };
static const int DX[4] = { 0, 0, -1, 1 };
static const int DY[4] = { -1, 1, 0, 0 };

typedef struct {
   vec3 pos, vel;
   float life, max;
   float r, g, b;
} Particle;

#define MAX_PARTICLES 400

typedef struct {
   Cell body[GW * GH + 2], prev[GW * GH + 2];
   int len, dir, turns[3], nturns;
   Cell food;
   double food_born, scored_at, state_at, acc;
   unsigned score, best;
   int new_best, state;
   float shake;
   Particle parts[MAX_PARTICLES];
   int nparts;
} Game;

static Game G;

static int cell_eq(Cell a, Cell b) { return a.x == b.x && a.y == b.y; }
static vec3 cell_world(Cell c, float y) { return v3(c.x + 0.5f, y, c.y + 0.5f); }
static int tick_ms(void) { int t = 130 - 3 * (int)G.score; return t < 55 ? 55 : t; }

static int on_body(Cell c, int skip_tail) {
   for (int i = 0; i < G.len - skip_tail; i++)
      if (cell_eq(G.body[i], c)) return 1;
   return 0;
}

static void spawn_food(double now) {
   for (;;) {
      Cell c = { rnd_range(0, GW), rnd_range(0, GH) };
      if (!on_body(c, 0)) { G.food = c; G.food_born = now; return; }
   }
}

static void burst(vec3 p, int n, float speed, float r, float g, float b) {
   for (int i = 0; i < n && G.nparts < MAX_PARTICLES; i++) {
      vec3 v;
      do { v = v3(rndf() * 2 - 1, rndf() * 2 - 1, rndf() * 2 - 1); } while (v3dot(v, v) > 1.0f);
      v = v3mul(v, speed);
      v.y = fabsf(v.y) + speed * 0.3f;
      float life = 0.5f + rndf() * 0.9f;
      int white = rnd() % 4 == 0;
      G.parts[G.nparts++] = (Particle){ p, v, life, life, white ? 1.0f : r, white ? 1.0f : g, white ? 1.0f : b };
   }
}

static void parts_update(float dt) {
   for (int i = 0; i < G.nparts; i++) {
      Particle *p = &G.parts[i];
      p->pos = v3add(p->pos, v3mul(p->vel, dt));
      p->vel.y -= 14.0f * dt;
      p->vel.x -= p->vel.x * 1.5f * dt;
      p->vel.z -= p->vel.z * 1.5f * dt;
      if (p->pos.y < 0.1f) { p->pos.y = 0.1f; p->vel.y = -p->vel.y * 0.45f; }
      p->life -= dt;
   }
   int w = 0;
   for (int i = 0; i < G.nparts; i++)
      if (G.parts[i].life > 0) G.parts[w++] = G.parts[i];
   G.nparts = w;
}

static void snake_color(int i, double now, float *r, float *g, float *b) {
   hsv((float)(now * 40.0) * 1.0f - i * 14.0f + 150.0f, 0.75f, 1.0f, r, g, b);
}

static void start_game(double now) {
   int cy = GH / 2;
   G.len = 4;
   for (int i = 0; i < 4; i++) G.body[i] = (Cell){ 8 - i, cy };
   memcpy(G.prev, G.body, sizeof G.body);
   G.dir = RIGHT;
   G.nturns = 0;
   G.score = 0;
   G.new_best = 0;
   G.acc = 0;
   G.shake = 0;
   spawn_food(now);
   G.state = S_PLAY;
   G.state_at = now;
}

static void turn(int d) {
   int last = G.nturns > 0 ? G.turns[G.nturns - 1] : G.dir;
   if (d != last && d != (last ^ 1) && G.nturns < 3) G.turns[G.nturns++] = d;
}

/* One grid step. False on a crash (the snake stays where it was). */
static int step(double now) {
   if (G.nturns > 0) {
      G.dir = G.turns[0];
      G.turns[0] = G.turns[1];
      G.turns[1] = G.turns[2];
      G.nturns--;
   }
   Cell head = { G.body[0].x + DX[G.dir], G.body[0].y + DY[G.dir] };
   int eating = cell_eq(head, G.food);
   int blocked = on_body(head, eating ? 0 : 1);
   memcpy(G.prev, G.body, sizeof G.body);
   if (head.x < 0 || head.y < 0 || head.x >= GW || head.y >= GH || blocked) return 0;
   Cell tail = G.body[G.len - 1];
   memmove(&G.body[1], &G.body[0], (G.len - 1) * sizeof(Cell));
   G.body[0] = head;
   if (eating) {
      G.body[G.len] = tail;
      G.prev[G.len] = tail;
      G.len++;
      G.score++;
      G.scored_at = now;
      float r, g, b;
      hsv((float)(now * 60.0), 0.6f, 1.0f, &r, &g, &b);
      burst(cell_world(G.food, 0.5f), 48, 4.5f, 1.0f, 0.24f, 0.5f);
      burst(cell_world(G.food, 0.5f), 12, 3.0f, r, g, b);
      spawn_food(now);
   }
   return 1;
}

/* The autopilot of the title screen and of unattended runs: the direction that keeps the most room, then the nearest food. */
static int reachable(Cell from, int cap) {
   static unsigned char seen[GW * GH];
   static Cell queue[GW * GH];
   memset(seen, 0, sizeof seen);
   for (int i = 0; i < G.len - 1; i++) seen[G.body[i].y * GW + G.body[i].x] = 1;
   int qh = 0, qt = 0, n = 0;
   queue[qt++] = from;
   seen[from.y * GW + from.x] = 1;
   while (qh < qt && n < cap) {
      Cell c = queue[qh++];
      n++;
      for (int d = 0; d < 4; d++) {
         Cell m = { c.x + DX[d], c.y + DY[d] };
         if (m.x < 0 || m.y < 0 || m.x >= GW || m.y >= GH || seen[m.y * GW + m.x]) continue;
         seen[m.y * GW + m.x] = 1;
         queue[qt++] = m;
      }
   }
   return n;
}

static int autopilot(void) {
   int best_d = G.dir, best = -1000000;
   for (int d = 0; d < 4; d++) {
      if (d == (G.dir ^ 1)) continue;
      Cell n = { G.body[0].x + DX[d], G.body[0].y + DY[d] };
      if (n.x < 0 || n.y < 0 || n.x >= GW || n.y >= GH || on_body(n, cell_eq(n, G.food) ? 0 : 1)) continue;
      int room = reachable(n, G.len + 12);
      int s = -(abs(n.x - G.food.x) + abs(n.y - G.food.y)) * 4 + (room < G.len + 4 ? -5000 + room : 0) + (d == G.dir ? 1 : 0);
      if (s > best) { best = s; best_d = d; }
   }
   return best_d;
}

/* ── Rendering: one object = one draw ───────────────────────────────────────────────────────────────────────────────────────── */

#define KIND_SPHERE 0
#define KIND_BOX 1
#define KIND_QUAD 2
#define MAT_LIT 0
#define MAT_FLOOR 1
#define MAT_GLOW 2
#define MAT_SHADOW 3
#define MAT_GLYPH 4

struct pc {   /* snake3d.vert's push constants, 160 bytes */
   float vp[16];
   float a[4], b[4], c[4], d[4], e[4];
   uint32_t g[4];
};

static struct {
   VkCommandBuffer cmd;
   PFN_vkCmdPushConstants push;
   PFN_vkCmdDraw draw;
   PFN_vkCmdBindPipeline bind;
   VkPipelineLayout layout;
   VkPipeline pipe[3];   /* opaque, additive, alpha */
   struct pc pc;
   int draws;
} R;

enum { P_OPAQUE, P_ADD, P_ALPHA };

static void use_pipe(int p) { R.bind(R.cmd, VK_PIPELINE_BIND_POINT_GRAPHICS, R.pipe[p]); }

static void object(int kind, vec3 c, vec3 ext, int mat, float cr, float cg, float cb, float w) {
   struct pc *p = &R.pc;
   p->a[0] = c.x; p->a[1] = c.y; p->a[2] = c.z; p->a[3] = (float)kind;
   p->b[0] = ext.x; p->b[1] = ext.y; p->b[2] = ext.z; p->b[3] = (float)mat;
   p->c[0] = cr; p->c[1] = cg; p->c[2] = cb; p->c[3] = w;
   R.push(R.cmd, R.layout, VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0, sizeof *p, p);
   R.draw(R.cmd, kind == KIND_SPHERE ? 20 * 12 * 6 : kind == KIND_BOX ? 36 : 6, 1, 0, 0);
   R.draws++;
}

static void sphere(vec3 c, float radius, int mat, float r, float g, float b, float w) {
   object(KIND_SPHERE, c, v3(radius, radius, radius), mat, r, g, b, w);
}

static uint32_t *glyph_rows(char ch, uint32_t out[2]) {
   const unsigned char *rows = NULL;
   if (ch >= 'a' && ch <= 'z') ch = (char)(ch - 'a' + 'A');
   for (unsigned i = 0; i < sizeof snake_font / sizeof snake_font[0]; i++)
      if (snake_font[i].ch == ch) rows = snake_font[i].rows;
   out[0] = out[1] = 0;
   if (!rows) return out;
   for (int r = 0; r < 7; r++)
      for (int c = 0; c < 5; c++)
         if (rows[r] & (1 << (4 - c))) {
            int bit = r * 5 + c;
            if (bit < 30) out[0] |= 1u << bit; else out[1] |= 1u << (bit - 30);
         }
   return out;
}

static float text_width(const char *s, float px) { return (float)strlen(s) * 6.0f * px - px; }

/* A glyph quad at pixel (x, y), `px` screen pixels per font pixel. */
static void glyph(char ch, float x, float y, float px, float z, float r, float g, float b) {
   if (ch == ' ') return;
   uint32_t bits[2];
   glyph_rows(ch, bits);
   R.pc.g[0] = bits[0]; R.pc.g[1] = bits[1];
   object(KIND_QUAD, v3(x, y, z), v3(5 * px, 7 * px, 0), MAT_GLYPH, r, g, b, 1.0f);
}

static void text(const char *s, float x, float y, float px, float r, float g, float b) {
   for (int i = 0; s[i]; i++) glyph(s[i], x + i * 6 * px + px * 0.5f, y + px * 0.5f, px, 0.02f, 0.0f, 0.0f, 0.02f);
   for (int i = 0; s[i]; i++) glyph(s[i], x + i * 6 * px, y, px, 0.01f, r, g, b);
}

static void text_center(const char *s, float cx, float y, float px, float r, float g, float b) {
   text(s, cx - text_width(s, px) / 2, y, px, r, g, b);
}

static void text_rainbow(const char *s, float cx, float y, float px, double now, float sat) {
   float x = cx - text_width(s, px) / 2;
   for (int i = 0; s[i]; i++) glyph(s[i], x + i * 6 * px + px * 0.6f, y + px * 0.6f, px, 0.02f, 0.0f, 0.0f, 0.02f);
   for (int i = 0; s[i]; i++) {
      float r, g, b;
      hsv((float)(now * 70.0) + i * 38.0f, sat, 1.0f, &r, &g, &b);
      float bob = sinf((float)now * 3.0f - i * 0.7f) * px * 0.5f;
      glyph(s[i], x + i * 6 * px, y + bob, px, 0.01f, r, g, b);
   }
}

static char *utoa(unsigned v, char *buf) {
   char tmp[12];
   int n = 0;
   do { tmp[n++] = (char)('0' + v % 10); v /= 10; } while (v);
   for (int i = 0; i < n; i++) buf[i] = tmp[n - 1 - i];
   buf[n] = 0;
   return buf;
}

/* Camera: overview or chase, smoothed. */
static struct { vec3 eye, target; int mode, valid; } CAM;

static void camera_update(double now, float dt, float aspect, mat4 *vp, vec3 *eye_out) {
   vec3 want_eye, want_target;
   vec3 center = v3(GW / 2.0f, 0.0f, GH / 2.0f);
   if (G.state == S_TITLE || G.state == S_OVER) {
      float a = (float)now * 0.22f;
      want_target = center;
      want_eye = v3(center.x + sinf(a) * 24.0f, 17.0f, center.z + cosf(a) * 20.0f);
   } else if (CAM.mode == 0) {
      vec3 head = cell_world(G.body[0], 0.0f);
      want_target = v3lerp(center, head, 0.22f);
      want_eye = v3(center.x + (head.x - center.x) * 0.25f, 17.5f, center.z + 12.5f);
   } else {
      vec3 head = v3lerp(cell_world(G.prev[0], 0.0f), cell_world(G.body[0], 0.0f), 1.0f);
      vec3 fwd = v3(DX[G.dir], 0, DY[G.dir]);
      want_target = v3add(head, v3add(v3mul(fwd, 3.5f), v3(0, 0.3f, 0)));
      want_eye = v3add(head, v3add(v3mul(fwd, -5.0f), v3(0, 4.2f, 0)));
   }
   if (!CAM.valid) { CAM.eye = want_eye; CAM.target = want_target; CAM.valid = 1; }
   float k = 1.0f - expf(-dt * (CAM.mode == 1 && G.state != S_TITLE ? 3.5f : 2.5f));
   CAM.eye = v3lerp(CAM.eye, want_eye, k);
   CAM.target = v3lerp(CAM.target, want_target, k);
   vec3 eye = CAM.eye, target = CAM.target;
   if (G.shake > 0) {
      eye = v3add(eye, v3(rndf() - 0.5f, rndf() - 0.5f, rndf() - 0.5f));
      eye = v3add(eye, v3mul(v3(rndf() - 0.5f, rndf() - 0.5f, rndf() - 0.5f), G.shake * 0.25f));
   }
   mat4 v = m4_lookat(eye, target, v3(0, 1, 0));
   mat4 p = m4_persp(0.82f, aspect, 0.1f, 200.0f);
   *vp = m4mul(&p, &v);
   *eye_out = eye;
}

/* Records one frame's draws into R.cmd (inside a rendering). `t` is how far through the step the snake is, 0..1. */
static void draw_scene(double now, float dt, float aspect, float sw, float sh, float t) {
   mat4 vp;
   vec3 eye;
   camera_update(now, dt, aspect, &vp, &eye);
   memcpy(R.pc.vp, vp.m, sizeof vp.m);

   float pulse = 0.5f + 0.5f * sinf((float)now * 5.0f);
   vec3 food_pos = cell_world(G.food, 0.62f + 0.1f * sinf((float)now * 2.6f));
   float born = clampf((float)(now - G.food_born) / 0.25f, 0.0f, 1.0f);
   int playing = G.state == S_PLAY || G.state == S_PAUSED || G.state == S_DYING || G.state == S_TITLE;
   R.pc.d[0] = eye.x; R.pc.d[1] = eye.y; R.pc.d[2] = eye.z; R.pc.d[3] = (float)now;
   R.pc.e[0] = food_pos.x; R.pc.e[1] = 1.2f; R.pc.e[2] = food_pos.z; R.pc.e[3] = (playing ? 1.0f : 0.0f) * born * (0.75f + 0.25f * pulse);

   /* floor and neon walls */
   use_pipe(P_OPAQUE);
   object(KIND_BOX, v3(GW / 2.0f, -0.25f, GH / 2.0f), v3(160, 0.25f, 160), MAT_FLOOR, 0, 0, 0, 0);
   float wr = 0.15f, wg = 0.6f, wb = 1.0f;
   object(KIND_BOX, v3(GW / 2.0f, 0.3f, -0.3f), v3(GW / 2.0f + 0.6f, 0.3f, 0.3f), MAT_LIT, wr, wg, wb, 0.7f);
   object(KIND_BOX, v3(GW / 2.0f, 0.3f, GH + 0.3f), v3(GW / 2.0f + 0.6f, 0.3f, 0.3f), MAT_LIT, wr, wg, wb, 0.7f);
   object(KIND_BOX, v3(-0.3f, 0.3f, GH / 2.0f), v3(0.3f, 0.3f, GH / 2.0f), MAT_LIT, wr, wg, wb, 0.7f);
   object(KIND_BOX, v3(GW + 0.3f, 0.3f, GH / 2.0f), v3(0.3f, 0.3f, GH / 2.0f), MAT_LIT, wr, wg, wb, 0.7f);

   /* where each segment is, gliding between its last two cells */
   vec3 seg[GW * GH + 2];
   int n = (G.state == S_OVER) ? 0 : G.len;
   for (int i = 0; i < n; i++) {
      float tt = (G.state == S_PLAY || G.state == S_PAUSED || G.state == S_TITLE) ? t : 1.0f;
      seg[i] = v3lerp(cell_world(G.prev[i], 0), cell_world(G.body[i], 0), tt);
   }
   float flash_r = -1, flash_g = 0, flash_b = 0;
   if (G.state == S_DYING) {
      int on = ((int)((now - G.state_at) / 0.09)) % 2 == 0;
      flash_r = on ? 1.0f : 1.0f; flash_g = on ? 1.0f : 0.16f; flash_b = on ? 1.0f : 0.29f;
   }
   #define RADIUS(i) (0.50f - 0.18f * (float)(i) / (float)(n > 8 ? n : 8))

   /* blob shadows on the floor */
   use_pipe(P_ALPHA);
   for (int i = 0; i < n; i++) {
      float r = RADIUS(i) * 1.5f;
      object(KIND_SPHERE, v3(seg[i].x + 0.12f, 0.03f, seg[i].z + 0.12f), v3(r, 0.01f, r), MAT_SHADOW, 0, 0, 0, 0.55f);
   }
   if (playing) {
      float r = 0.5f * born;
      object(KIND_SPHERE, v3(food_pos.x, 0.03f, food_pos.z), v3(r * 1.6f, 0.01f, r * 1.6f), MAT_SHADOW, 0, 0, 0, 0.5f);
   }

   /* the snake: each link between segments is three spheres, so it reads as a tube */
   use_pipe(P_OPAQUE);
   for (int i = n - 1; i >= 0; i--) {
      float r, g, b;
      if (flash_r >= 0) { r = flash_r; g = flash_g; b = flash_b; }
      else snake_color(i, now, &r, &g, &b);
      float rad = RADIUS(i);
      if (i > 0) {
         for (int k = 1; k <= 2; k++) {
            float f = k / 3.0f, rr = lerpf(rad, RADIUS(i - 1), f);
            float r2, g2, b2;
            if (flash_r >= 0) { r2 = flash_r; g2 = flash_g; b2 = flash_b; } else snake_color(i, now, &r2, &g2, &b2);
            sphere(v3(lerpf(seg[i].x, seg[i - 1].x, f), 0.46f, lerpf(seg[i].z, seg[i - 1].z, f)), rr, MAT_LIT, r2, g2, b2, flash_r >= 0 ? 0.5f : 0.06f);
         }
      }
      sphere(v3(seg[i].x, 0.46f, seg[i].z), i == 0 ? 0.6f : rad, MAT_LIT, r, g, b, flash_r >= 0 ? 0.5f : 0.06f);
   }
   if (n > 0) {   /* eyes that look where the snake goes */
      vec3 fwd = v3(DX[G.dir], 0, DY[G.dir]), side = v3(-DY[G.dir], 0, DX[G.dir]);
      for (int s = -1; s <= 1; s += 2) {
         vec3 e = v3add(seg[0], v3add(v3mul(fwd, 0.30f), v3add(v3mul(side, 0.27f * s), v3(0, 0.62f, 0))));
         sphere(e, 0.17f, MAT_LIT, 1.0f, 1.0f, 1.0f, 0.3f);
         sphere(v3add(e, v3add(v3mul(fwd, 0.10f), v3(0, 0.02f, 0))), 0.09f, MAT_LIT, 0.03f, 0.03f, 0.06f, 0.0f);
      }
   }

   /* the food: a glowing orb with three satellites */
   if (playing) {
      float r = (0.30f + 0.05f * pulse) * born;
      sphere(food_pos, r, MAT_LIT, 1.0f, 0.24f, 0.5f, 1.6f);
      for (int k = 0; k < 3; k++) {
         float a = (float)now * 2.2f + k * 2.0944f;
         vec3 s = v3add(food_pos, v3(cosf(a) * 0.55f, sinf(a * 1.3f) * 0.12f, sinf(a) * 0.55f));
         sphere(s, 0.07f * born, MAT_LIT, 1.0f, 0.9f, 0.6f, 2.0f);
      }
   }

   /* additive: halos and particles, tested against the depth but not written */
   use_pipe(P_ADD);
   if (playing) {
      sphere(food_pos, (1.0f + 0.18f * pulse) * born, MAT_GLOW, 1.0f, 0.24f, 0.5f, 0.65f);
   }
   if (n > 0 && flash_r < 0) {
      float r, g, b;
      snake_color(0, now, &r, &g, &b);
      sphere(seg[0], 1.25f, MAT_GLOW, r, g, b, 0.22f);
   }
   for (int i = 0; i < G.nparts; i++) {
      Particle *p = &G.parts[i];
      float f = p->life / p->max;
      sphere(p->pos, 0.06f + 0.06f * f, MAT_GLOW, p->r, p->g, p->b, 1.5f * f);
   }

   /* HUD, in pixels, in front of everything */
   mat4 px = m4_pixels(sw, sh);
   memcpy(R.pc.vp, px.m, sizeof px.m);
   use_pipe(P_OPAQUE);
   float u = sh / 150.0f;   /* a font pixel: about 1/21 of the screen's height per glyph row */
   char buf[16];
   if (G.state != S_TITLE) {
      float pop = clampf((float)(now - G.scored_at) / 0.3f, 0.0f, 1.0f);
      text("SCORE", sw * 0.025f, sh * 0.03f, u * 0.9f, 0.5f, 0.53f, 0.69f);
      float sx = sw * 0.025f + text_width("SCORE ", u * 0.9f);
      text(utoa(G.score, buf), sx, sh * 0.03f, u * 0.9f, lerpf(1.0f, 1.0f, pop), lerpf(0.4f, 1.0f, pop), lerpf(0.6f, 1.0f, pop));
      char bb[16];
      utoa(G.best, bb);
      float bx = sw * 0.975f - text_width(bb, u * 0.9f);
      text(bb, bx, sh * 0.03f, u * 0.9f, 1.0f, 0.83f, 0.36f);
      text("BEST", bx - text_width("BEST ", u * 0.9f), sh * 0.03f, u * 0.9f, 0.5f, 0.53f, 0.69f);
   }
   float cx = sw / 2;
   if (G.state == S_TITLE) {
      text_rainbow("SNAKE", cx, sh * 0.08f, u * 3.6f, now, 0.65f);
      text_rainbow("3D", cx, sh * 0.08f + u * 3.6f * 8.5f, u * 3.0f, now + 2.0, 0.5f);
      if (((int)(now * 2.0)) % 2 == 0) text_center("PRESS SPACE TO PLAY", cx, sh * 0.76f, u * 1.2f, 1, 1, 1);
      text_center("ARROWS/WASD MOVE - P PAUSE - C CAMERA - ESC QUIT", cx, sh * 0.93f, u * 0.6f, 0.5f, 0.53f, 0.69f);
      if (G.best > 0) {
         char bb[24] = "BEST ";
         utoa(G.best, bb + 5);
         text_center(bb, cx, sh * 0.85f, u * 0.9f, 1.0f, 0.83f, 0.36f);
      }
   } else if (G.state == S_PAUSED) {
      text_center("PAUSED", cx, sh * 0.4f, u * 3.0f, 1, 1, 1);
      text_center("P TO RESUME", cx, sh * 0.6f, u * 0.8f, 0.5f, 0.53f, 0.69f);
   } else if (G.state == S_OVER) {
      text_rainbow("GAME OVER", cx, sh * 0.2f, u * 2.6f, now, 0.8f);
      char line[24] = "SCORE ";
      utoa(G.score, line + 6);
      text_center(line, cx, sh * 0.48f, u * 1.6f, 1, 1, 1);
      if (G.new_best) {
         float r, g, b;
         hsv((float)(now * 300.0), 0.6f, 1.0f, &r, &g, &b);
         text_center("NEW BEST!", cx, sh * 0.62f, u * 1.2f, r, g, b);
      }
      if (((int)(now * 2.0)) % 2 == 0) text_center("SPACE TO PLAY AGAIN", cx, sh * 0.78f, u * 1.0f, 1, 1, 1);
   }
}

/* ── Vulkan ─────────────────────────────────────────────────────────────────────────────────────────────────────────────────── */

static int failures;
#define VKOK(call) do { VkResult r_ = (call); if (r_ != VK_SUCCESS) { failures++; printf("SNAKE3D FAIL %s -> %d (line %d)\n", #call, (int)r_, __LINE__); goto done; } } while (0)

#define GLOBAL(name) PFN_##name name = (PFN_##name)GET_INSTANCE_PROC(NULL, #name)
#define INST(name) PFN_##name name = (PFN_##name)GET_INSTANCE_PROC(instance, #name)
#define DEV(name) PFN_##name name = (PFN_##name)vkGetDeviceProcAddr(device, #name)

static int find_type(const VkPhysicalDeviceMemoryProperties *mp, uint32_t allowed, VkMemoryPropertyFlags want, VkMemoryPropertyFlags avoid) {
   for (uint32_t i = 0; i < mp->memoryTypeCount; i++)
      if ((allowed & (1u << i)) && (mp->memoryTypes[i].propertyFlags & want) == want && !(mp->memoryTypes[i].propertyFlags & avoid))
         return (int)i;
   return -1;
}

static double clock_s(void) {
   struct timespec ts;
   clock_gettime(CLOCK_MONOTONIC, &ts);
   return ts.tv_sec + ts.tv_nsec / 1e9;
}

#ifndef SNAKE_HOST
#define EVIOCGRAB 0x40044590u
#define KEY_ESC 1
#define KEY_Q 16
#define KEY_W 17
#define KEY_P 25
#define KEY_ENTER 28
#define KEY_A 30
#define KEY_S 31
#define KEY_D 32
#define KEY_C 46
#define KEY_SPACE 57
#define KEY_UP 103
#define KEY_LEFT 105
#define KEY_RIGHT 106
#define KEY_DOWN 108
#endif

struct input { int fd; };

static void input_open(struct input *in) {
#ifndef SNAKE_HOST
   in->fd = open("/dev/input/event0", O_RDONLY | O_NONBLOCK);
   if (in->fd >= 0) {
      ioctl(in->fd, EVIOCGRAB, 1);
      char rec[24];
      while (read(in->fd, rec, sizeof rec) == (ssize_t)sizeof rec) {}   /* the ring holds every key since boot, and the Enter that started us */
   }
#else
   in->fd = -1;
#endif
}

static void input_close(struct input *in) {
#ifndef SNAKE_HOST
   if (in->fd >= 0) { ioctl(in->fd, EVIOCGRAB, 0); close(in->fd); }
#else
   (void)in;
#endif
}

#ifndef SNAKE_HOST
/* One key event (a Linux KEY_* code, value 1 press / 2 autorepeat / 0 release), from evdev or from the window. Returns 1 to quit. */
static int key_apply(uint16_t code, int32_t value, double now) {
   if (value == 0) return 0;
   int fresh = value == 1;   /* not an autorepeat */
   int dir = -1;
   switch (code) {
   case KEY_UP: case KEY_W: dir = UP; break;
   case KEY_DOWN: case KEY_S: dir = DOWN; break;
   case KEY_LEFT: case KEY_A: dir = LEFT; break;
   case KEY_RIGHT: case KEY_D: dir = RIGHT; break;
   }
   if ((code == KEY_ESC || code == KEY_Q) && fresh) return 1;
   if ((G.state == S_TITLE || G.state == S_OVER) && (code == KEY_SPACE || code == KEY_ENTER) && fresh) start_game(now);
   else if (G.state == S_PLAY && code == KEY_P && fresh) G.state = S_PAUSED;
   else if (G.state == S_PAUSED && (code == KEY_P || code == KEY_SPACE || code == KEY_ENTER) && fresh) G.state = S_PLAY;
   else if (code == KEY_C && fresh) CAM.mode ^= 1;
   else if (G.state == S_PLAY && dir >= 0) turn(dir);
   return 0;
}
#endif

/* Applies the keys typed since the last call. Returns 1 to quit. */
static int input_poll(struct input *in, double now) {
#ifndef SNAKE_HOST
   char rec[24];
   while (in->fd >= 0 && read(in->fd, rec, sizeof rec) == (ssize_t)sizeof rec) {
      const unsigned char *u = (const unsigned char *)rec;
      uint16_t type = (uint16_t)(u[16] | u[17] << 8), code = (uint16_t)(u[18] | u[19] << 8);
      int32_t value;
      memcpy(&value, rec + 20, 4);
      if (type != 1) continue;
      if (key_apply(code, value, now)) return 1;
   }
#else
   (void)in; (void)now;
#endif
   return 0;
}

int main(void) {
   setvbuf(stdout, NULL, _IONBF, 0);
   const char *autoplay_env = getenv("SNAKE3D_AUTOPLAY");
   int autoplay = autoplay_env && *autoplay_env == '1';
   const char *secs_env = getenv("SNAKE3D_SECONDS");
   double run_seconds = secs_env ? atof(secs_env) : 0;
#ifdef SNAKE_HOST
   const char *dump_dir = getenv("SNAKE3D_DUMP");
   const char *dump_at = getenv("SNAKE3D_DUMP_AT");
   const char *frames_env = getenv("SNAKE3D_FRAMES");
   int host_frames = frames_env ? atoi(frames_env) : 600;
   const char *die_env = getenv("SNAKE3D_DIE_AT");
   int die_at = die_env ? atoi(die_env) : -1;
   const char *start_env = getenv("SNAKE3D_START_AT");
   int start_at = start_env ? atoi(start_env) : 90;
   const char *cam_env = getenv("SNAKE3D_CAMERA");
   if (cam_env) CAM.mode = atoi(cam_env);
   const char *seed_env = getenv("SNAKE3D_SEED");
   if (seed_env) rng_state = (uint32_t)atoi(seed_env) | 1;
#endif
   { struct timespec ts; clock_gettime(CLOCK_REALTIME, &ts); if (!getenv("SNAKE3D_SEED")) rng_state ^= (uint32_t)ts.tv_nsec | 1; }

   VkInstance instance = VK_NULL_HANDLE;
   VkDevice device = VK_NULL_HANDLE;
   PFN_vkGetDeviceProcAddr vkGetDeviceProcAddr = NULL;
   VkImage image = VK_NULL_HANDLE, dimage = VK_NULL_HANDLE;
   VkDeviceMemory imem = VK_NULL_HANDLE, dmem = VK_NULL_HANDLE;
   VkImageView view = VK_NULL_HANDLE, dview = VK_NULL_HANDLE;
#ifdef SNAKE_HOST
   VkDeviceMemory smem[3] = { VK_NULL_HANDLE, VK_NULL_HANDLE, VK_NULL_HANDLE };
   VkBuffer sbuf[3] = { VK_NULL_HANDLE, VK_NULL_HANDLE, VK_NULL_HANDLE };
#else
   enum { MAX_SC_IMAGES = 8 };
   VkSurfaceKHR surface = VK_NULL_HANDLE;
   VkSwapchainKHR swapchain = VK_NULL_HANDLE;
   VkImage sc_images[MAX_SC_IMAGES];
   VkImageView sc_views[MAX_SC_IMAGES] = { VK_NULL_HANDLE };
   VkSemaphore render_sem[MAX_SC_IMAGES] = { VK_NULL_HANDLE };   /* one per image: a present may still be waiting on the last one signalled */
   VkSemaphore acquire_sem = VK_NULL_HANDLE;
   uint32_t sc_count = 0;
   struct gvk_window win;
   int windowed = getenv("SNAKE3D_WINDOW") != NULL, have_window = 0, made_target = 0;
   unsigned resizes = 0;
#endif
   VkShaderModule vmod = VK_NULL_HANDLE, fmod = VK_NULL_HANDLE;
   VkPipelineLayout pl = VK_NULL_HANDLE;
   VkCommandPool cpool = VK_NULL_HANDLE;
   VkFence fence = VK_NULL_HANDLE;
   struct input in = { -1 };
   int have_input = 0;
#ifdef SNAKE_HOST
   uint8_t *smap[3] = { NULL, NULL, NULL };
#endif

   GLOBAL(vkCreateInstance);
   if (!vkCreateInstance) return 1;
   VkApplicationInfo app = { .sType = VK_STRUCTURE_TYPE_APPLICATION_INFO, .pApplicationName = "snake3d", .apiVersion = VK_API_VERSION_1_3 };
   VkInstanceCreateInfo ici = { .sType = VK_STRUCTURE_TYPE_INSTANCE_CREATE_INFO, .pApplicationInfo = &app };
#ifndef SNAKE_HOST
   static const char *const instance_exts[] = { "VK_KHR_surface", "VK_EXT_headless_surface" };
   ici.enabledExtensionCount = 2;
   ici.ppEnabledExtensionNames = instance_exts;
#endif
   VKOK(vkCreateInstance(&ici, NULL, &instance));
   INST(vkEnumeratePhysicalDevices);
   INST(vkGetPhysicalDeviceQueueFamilyProperties);
   INST(vkGetPhysicalDeviceMemoryProperties);
   INST(vkGetPhysicalDeviceProperties);
   INST(vkCreateDevice);
   vkGetDeviceProcAddr = (PFN_vkGetDeviceProcAddr)GET_INSTANCE_PROC(instance, "vkGetDeviceProcAddr");

   uint32_t npd = 8;
   VkPhysicalDevice pdevs[8];
   VkResult r = vkEnumeratePhysicalDevices(instance, &npd, pdevs);
   if ((r != VK_SUCCESS && r != VK_INCOMPLETE) || npd == 0) { printf("SNAKE3D FAIL no physical device (%d)\n", (int)r); failures++; goto done; }
   VkPhysicalDevice pdev = pdevs[0];
   VkPhysicalDeviceProperties props;
   vkGetPhysicalDeviceProperties(pdev, &props);
   printf("SNAKE3D using %s\n", props.deviceName);
   if (props.limits.maxPushConstantsSize < sizeof(struct pc)) { printf("SNAKE3D FAIL push constants of %u bytes, need %zu\n", props.limits.maxPushConstantsSize, sizeof(struct pc)); failures++; goto done; }

   uint32_t nq = 0;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, NULL);
   VkQueueFamilyProperties qf[16];
   if (nq > 16) nq = 16;
   vkGetPhysicalDeviceQueueFamilyProperties(pdev, &nq, qf);
   int family = -1;
   for (uint32_t i = 0; i < nq; i++)
      if (family < 0 && (qf[i].queueFlags & VK_QUEUE_GRAPHICS_BIT)) family = (int)i;
   if (family < 0) { printf("SNAKE3D FAIL no graphics queue family\n"); failures++; goto done; }

   VkPhysicalDeviceMemoryProperties mp;
   vkGetPhysicalDeviceMemoryProperties(pdev, &mp);

   float prio = 1.0f;
   VkDeviceQueueCreateInfo qci = { .sType = VK_STRUCTURE_TYPE_DEVICE_QUEUE_CREATE_INFO, .queueFamilyIndex = (uint32_t)family, .queueCount = 1, .pQueuePriorities = &prio };
   VkPhysicalDeviceVulkan13Features f13 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_3_FEATURES, .dynamicRendering = VK_TRUE };
   VkPhysicalDeviceVulkan12Features f12 = { .sType = VK_STRUCTURE_TYPE_PHYSICAL_DEVICE_VULKAN_1_2_FEATURES, .pNext = &f13, .timelineSemaphore = VK_TRUE };
   VkDeviceCreateInfo dci = { .sType = VK_STRUCTURE_TYPE_DEVICE_CREATE_INFO, .pNext = &f12, .queueCreateInfoCount = 1, .pQueueCreateInfos = &qci };
#ifndef SNAKE_HOST
   static const char *const device_exts[] = { "VK_KHR_swapchain" };
   dci.enabledExtensionCount = 1;
   dci.ppEnabledExtensionNames = device_exts;
#endif
   VKOK(vkCreateDevice(pdev, &dci, NULL, &device));

   DEV(vkDestroyDevice); DEV(vkGetDeviceQueue); DEV(vkCreateBuffer); DEV(vkGetBufferMemoryRequirements); DEV(vkAllocateMemory);
   DEV(vkBindBufferMemory); DEV(vkMapMemory); DEV(vkCreateImage); DEV(vkGetImageMemoryRequirements); DEV(vkBindImageMemory);
   DEV(vkCreateImageView); DEV(vkCreateShaderModule); DEV(vkCreatePipelineLayout); DEV(vkCreateGraphicsPipelines);
   DEV(vkCreateCommandPool); DEV(vkAllocateCommandBuffers); DEV(vkBeginCommandBuffer); DEV(vkEndCommandBuffer);
   DEV(vkCmdBeginRendering); DEV(vkCmdEndRendering); DEV(vkCmdBindPipeline); DEV(vkCmdSetViewport); DEV(vkCmdSetScissor); DEV(vkCmdDraw);
   DEV(vkCmdPipelineBarrier); DEV(vkCmdCopyImageToBuffer); DEV(vkCreateFence); DEV(vkQueueSubmit); DEV(vkWaitForFences);
   DEV(vkDeviceWaitIdle); DEV(vkResetFences); DEV(vkResetCommandBuffer); DEV(vkCmdPushConstants);
   DEV(vkDestroyImageView); DEV(vkDestroyImage); DEV(vkFreeMemory);
#ifndef SNAKE_HOST
   DEV(vkCreateSwapchainKHR); DEV(vkGetSwapchainImagesKHR); DEV(vkAcquireNextImageKHR); DEV(vkQueuePresentKHR); DEV(vkCreateSemaphore);
   DEV(vkDestroySwapchainKHR); DEV(vkDestroySemaphore);
#endif
   VkQueue queue;
   vkGetDeviceQueue(device, (uint32_t)family, 0, &queue);

   /* the screen: its size (the surface says it when there is a display; the swapchain must have exactly that size) */
   uint32_t SW, SH;
   VkMemoryRequirements req;
#ifdef SNAKE_HOST
   struct nvg_scanout_info si = { .width = 1280, .height = 720, .pitch_B = 1280 * 4, .size_B = 1280 * 720 * 4 };
   if (getenv("SNAKE3D_SIZE")) sscanf(getenv("SNAKE3D_SIZE"), "%ux%u", &si.width, &si.height), si.pitch_B = si.width * 4, si.size_B = (uint64_t)si.pitch_B * si.height;
   SW = si.width;
   SH = si.height;
   printf("SNAKE3D screen %ux%u (offscreen)\n", SW, SH);
#else
   INST(vkCreateHeadlessSurfaceEXT);
   INST(vkGetPhysicalDeviceSurfaceSupportKHR);
   INST(vkGetPhysicalDeviceSurfaceCapabilitiesKHR);
   if (windowed) {
      /* a window of the compositor: the connection is ours, the surface is made of it (constanos_gui_vk.h) */
      if (gvk_open(&win, "snake3d") != 0) { printf("SNAKE3D FAIL no compositor to connect to ($GUI_DISPLAY)\n"); failures++; goto done; }
      have_window = 1;
      if (gvk_surface_create(&win, instance, &surface) != 0) { printf("SNAKE3D FAIL the window's surface\n"); failures++; goto done; }
      gvk_set_resizable(&win, 320, 180);   /* any size from here up: maximize, a resize drag and F11 work */
   } else {
      VkHeadlessSurfaceCreateInfoEXT hsci = { .sType = VK_STRUCTURE_TYPE_HEADLESS_SURFACE_CREATE_INFO_EXT };
      VKOK(vkCreateHeadlessSurfaceEXT(instance, &hsci, NULL, &surface));
   }
   VkBool32 supported = VK_FALSE;
   VKOK(vkGetPhysicalDeviceSurfaceSupportKHR(pdev, (uint32_t)family, surface, &supported));
   if (!supported) { printf("SNAKE3D FAIL the queue family cannot present\n"); failures++; goto done; }
   VkSurfaceCapabilitiesKHR caps;
   VKOK(vkGetPhysicalDeviceSurfaceCapabilitiesKHR(pdev, surface, &caps));
   /* currentExtent is -1 when the surface has no size of its own: no display behind it (QEMU's software device) */
   const int has_display = !windowed && caps.currentExtent.width != 0xFFFFFFFFu;
   int headless = getenv("SNAKE3D_HEADLESS") != NULL;   /* nothing to show it on: render and present into the void */
   if (!has_display && !headless && !windowed) { printf("SNAKE3D FAIL no display behind the surface: is gpu=uapi on, and nothing else holding the screen? (SNAKE3D_HEADLESS=1 to render anyway)\n"); failures++; goto done; }
   if (windowed) {
      const char *w_env = getenv("SNAKE3D_W"), *h_env = getenv("SNAKE3D_H");
      SW = w_env ? (uint32_t)atoi(w_env) : (uint32_t)win.cfg_w;
      SH = h_env ? (uint32_t)atoi(h_env) : (uint32_t)win.cfg_h;
      if (SW < 64 || SH < 64) { SW = 960; SH = 540; }
      printf("SNAKE3D window %ux%u (the compositor suggested %dx%d)\n", SW, SH, win.cfg_w, win.cfg_h);
   } else if (has_display) {
      SW = caps.currentExtent.width;
      SH = caps.currentExtent.height;
   } else {
      SW = 640;
      SH = 360;
      printf("SNAKE3D headless: the surface has no display, rendering %ux%u without showing it\n", SW, SH);
   }
   if (!windowed) printf("SNAKE3D screen %ux%u (%s)\n", SW, SH, has_display ? "the display" : "headless");
#endif

   const VkFormat DEPTH_FORMAT = VK_FORMAT_D32_SFLOAT;
   /* pipelines: one shader pair, three blend/depth states */
   VkShaderModuleCreateInfo vs = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = snake3d_vert_spv_len, .pCode = (const uint32_t *)snake3d_vert_spv };
   VkShaderModuleCreateInfo fs = { .sType = VK_STRUCTURE_TYPE_SHADER_MODULE_CREATE_INFO, .codeSize = snake3d_frag_spv_len, .pCode = (const uint32_t *)snake3d_frag_spv };
   VKOK(vkCreateShaderModule(device, &vs, NULL, &vmod));
   VKOK(vkCreateShaderModule(device, &fs, NULL, &fmod));
   VkPushConstantRange pcr = { VK_SHADER_STAGE_VERTEX_BIT | VK_SHADER_STAGE_FRAGMENT_BIT, 0, sizeof(struct pc) };
   VkPipelineLayoutCreateInfo plci = { .sType = VK_STRUCTURE_TYPE_PIPELINE_LAYOUT_CREATE_INFO, .pushConstantRangeCount = 1, .pPushConstantRanges = &pcr };
   VKOK(vkCreatePipelineLayout(device, &plci, NULL, &pl));
   VkPipelineShaderStageCreateInfo stages[2] = {
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_VERTEX_BIT, .module = vmod, .pName = "main" },
      { .sType = VK_STRUCTURE_TYPE_PIPELINE_SHADER_STAGE_CREATE_INFO, .stage = VK_SHADER_STAGE_FRAGMENT_BIT, .module = fmod, .pName = "main" } };
   VkPipelineVertexInputStateCreateInfo vi = { .sType = VK_STRUCTURE_TYPE_PIPELINE_VERTEX_INPUT_STATE_CREATE_INFO };
   VkPipelineInputAssemblyStateCreateInfo ia = { .sType = VK_STRUCTURE_TYPE_PIPELINE_INPUT_ASSEMBLY_STATE_CREATE_INFO, .topology = VK_PRIMITIVE_TOPOLOGY_TRIANGLE_LIST };
   VkPipelineViewportStateCreateInfo vp = { .sType = VK_STRUCTURE_TYPE_PIPELINE_VIEWPORT_STATE_CREATE_INFO, .viewportCount = 1, .scissorCount = 1 };
   VkPipelineRasterizationStateCreateInfo rs = { .sType = VK_STRUCTURE_TYPE_PIPELINE_RASTERIZATION_STATE_CREATE_INFO, .polygonMode = VK_POLYGON_MODE_FILL,
      .cullMode = VK_CULL_MODE_NONE, .frontFace = VK_FRONT_FACE_COUNTER_CLOCKWISE, .lineWidth = 1.0f };
   VkPipelineMultisampleStateCreateInfo ms = { .sType = VK_STRUCTURE_TYPE_PIPELINE_MULTISAMPLE_STATE_CREATE_INFO, .rasterizationSamples = VK_SAMPLE_COUNT_1_BIT };
   VkDynamicState dyn[2] = { VK_DYNAMIC_STATE_VIEWPORT, VK_DYNAMIC_STATE_SCISSOR };
   VkPipelineDynamicStateCreateInfo ds = { .sType = VK_STRUCTURE_TYPE_PIPELINE_DYNAMIC_STATE_CREATE_INFO, .dynamicStateCount = 2, .pDynamicStates = dyn };
   VkFormat cf = VK_FORMAT_B8G8R8A8_UNORM;
   VkPipelineRenderingCreateInfo pri = { .sType = VK_STRUCTURE_TYPE_PIPELINE_RENDERING_CREATE_INFO, .colorAttachmentCount = 1, .pColorAttachmentFormats = &cf,
      .depthAttachmentFormat = DEPTH_FORMAT };
   VkPipelineColorBlendAttachmentState cba[3] = {
      { .colorWriteMask = 0xf },
      { .blendEnable = VK_TRUE, .srcColorBlendFactor = VK_BLEND_FACTOR_ONE, .dstColorBlendFactor = VK_BLEND_FACTOR_ONE, .colorBlendOp = VK_BLEND_OP_ADD,
        .srcAlphaBlendFactor = VK_BLEND_FACTOR_ZERO, .dstAlphaBlendFactor = VK_BLEND_FACTOR_ONE, .alphaBlendOp = VK_BLEND_OP_ADD, .colorWriteMask = 0xf },
      { .blendEnable = VK_TRUE, .srcColorBlendFactor = VK_BLEND_FACTOR_SRC_ALPHA, .dstColorBlendFactor = VK_BLEND_FACTOR_ONE_MINUS_SRC_ALPHA, .colorBlendOp = VK_BLEND_OP_ADD,
        .srcAlphaBlendFactor = VK_BLEND_FACTOR_ZERO, .dstAlphaBlendFactor = VK_BLEND_FACTOR_ONE, .alphaBlendOp = VK_BLEND_OP_ADD, .colorWriteMask = 0xf } };
   for (int p = 0; p < 3; p++) {
      VkPipelineColorBlendStateCreateInfo cb = { .sType = VK_STRUCTURE_TYPE_PIPELINE_COLOR_BLEND_STATE_CREATE_INFO, .attachmentCount = 1, .pAttachments = &cba[p] };
      VkPipelineDepthStencilStateCreateInfo dss = { .sType = VK_STRUCTURE_TYPE_PIPELINE_DEPTH_STENCIL_STATE_CREATE_INFO, .depthTestEnable = VK_TRUE,
         .depthWriteEnable = p == P_OPAQUE ? VK_TRUE : VK_FALSE, .depthCompareOp = VK_COMPARE_OP_LESS };
      VkGraphicsPipelineCreateInfo gpci = { .sType = VK_STRUCTURE_TYPE_GRAPHICS_PIPELINE_CREATE_INFO, .pNext = &pri, .stageCount = 2, .pStages = stages,
         .pVertexInputState = &vi, .pInputAssemblyState = &ia, .pViewportState = &vp, .pRasterizationState = &rs, .pMultisampleState = &ms,
         .pDepthStencilState = &dss, .pColorBlendState = &cb, .pDynamicState = &ds, .layout = pl };
      VKOK(vkCreateGraphicsPipelines(device, VK_NULL_HANDLE, 1, &gpci, NULL, &R.pipe[p]));
   }

   VkCommandPoolCreateInfo cpi = { .sType = VK_STRUCTURE_TYPE_COMMAND_POOL_CREATE_INFO, .queueFamilyIndex = (uint32_t)family };
   VKOK(vkCreateCommandPool(device, &cpi, NULL, &cpool));
   VkCommandBufferAllocateInfo cbai = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_ALLOCATE_INFO, .commandPool = cpool, .level = VK_COMMAND_BUFFER_LEVEL_PRIMARY, .commandBufferCount = 1 };
   VKOK(vkAllocateCommandBuffers(device, &cbai, &R.cmd));
   VkFenceCreateInfo fci = { .sType = VK_STRUCTURE_TYPE_FENCE_CREATE_INFO };
   VKOK(vkCreateFence(device, &fci, NULL, &fence));
   R.push = vkCmdPushConstants; R.draw = vkCmdDraw; R.bind = vkCmdBindPipeline; R.layout = pl;

#ifndef SNAKE_HOST
   if (!windowed) {   /* a window gets its keys over the connection */
      input_open(&in);
      have_input = 1;
   }
#endif
   G.best = 0;
   G.state = S_TITLE;
   G.len = 6;
   for (int i = 0; i < 6; i++) G.body[i] = (Cell){ 10 - i, GH / 2 };
   memcpy(G.prev, G.body, sizeof G.body);
   G.dir = RIGHT;
   G.scored_at = -10;
   spawn_food(0);

   double t0 = clock_s(), last = t0;
   unsigned frames = 0, shown = 0, draws_max = 0;
   int quit = 0, frame_ok = 1, present_ok = 1;
   /* the target: the depth image and the swapchain with its views, at SW x SH. Made again (a jump back here) when the window's size changes. */
make_target:
   ;
#ifndef SNAKE_HOST
   if (made_target) {
      VKOK(vkDeviceWaitIdle(device));
      for (uint32_t i = 0; i < sc_count; i++) {
         vkDestroyImageView(device, sc_views[i], NULL); sc_views[i] = VK_NULL_HANDLE;
         vkDestroySemaphore(device, render_sem[i], NULL); render_sem[i] = VK_NULL_HANDLE;
      }
      vkDestroyImageView(device, dview, NULL); dview = VK_NULL_HANDLE;
      vkDestroyImage(device, dimage, NULL); dimage = VK_NULL_HANDLE;
      vkFreeMemory(device, dmem, NULL); dmem = VK_NULL_HANDLE;
   }
   made_target = 1;
#endif
   /* depth image, and the colour target: one image drawn offscreen and copied to a buffer (host), or the swapchain's images (the WSI copies them) */
   VkImageCreateInfo imci = { .sType = VK_STRUCTURE_TYPE_IMAGE_CREATE_INFO, .imageType = VK_IMAGE_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
      .extent = { SW, SH, 1 }, .mipLevels = 1, .arrayLayers = 1, .samples = VK_SAMPLE_COUNT_1_BIT, .tiling = VK_IMAGE_TILING_OPTIMAL,
      .usage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT | VK_IMAGE_USAGE_TRANSFER_SRC_BIT, .sharingMode = VK_SHARING_MODE_EXCLUSIVE,
      .initialLayout = VK_IMAGE_LAYOUT_UNDEFINED };
   VkImageViewCreateInfo ivci = { .sType = VK_STRUCTURE_TYPE_IMAGE_VIEW_CREATE_INFO, .viewType = VK_IMAGE_VIEW_TYPE_2D, .format = VK_FORMAT_B8G8R8A8_UNORM,
      .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };

   VkImageCreateInfo dici = imci;
   dici.format = DEPTH_FORMAT;
   dici.usage = VK_IMAGE_USAGE_DEPTH_STENCIL_ATTACHMENT_BIT;
   VKOK(vkCreateImage(device, &dici, NULL, &dimage));
   vkGetImageMemoryRequirements(device, dimage, &req);
   int it = find_type(&mp, req.memoryTypeBits, VK_MEMORY_PROPERTY_DEVICE_LOCAL_BIT, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT);
   if (it < 0) { printf("SNAKE3D FAIL no device-local memory type\n"); failures++; goto done; }
   VkMemoryAllocateInfo mai = { .sType = VK_STRUCTURE_TYPE_MEMORY_ALLOCATE_INFO, .allocationSize = req.size, .memoryTypeIndex = (uint32_t)it };
   VKOK(vkAllocateMemory(device, &mai, NULL, &dmem));
   VKOK(vkBindImageMemory(device, dimage, dmem, 0));
   VkImageViewCreateInfo divci = ivci;
   divci.image = dimage;
   divci.format = DEPTH_FORMAT;
   divci.subresourceRange.aspectMask = VK_IMAGE_ASPECT_DEPTH_BIT;
   VKOK(vkCreateImageView(device, &divci, NULL, &dview));

#ifdef SNAKE_HOST
   VKOK(vkCreateImage(device, &imci, NULL, &image));
   vkGetImageMemoryRequirements(device, image, &req);
   mai.allocationSize = req.size;
   VKOK(vkAllocateMemory(device, &mai, NULL, &imem));
   VKOK(vkBindImageMemory(device, image, imem, 0));
   ivci.image = image;
   VKOK(vkCreateImageView(device, &ivci, NULL, &view));

   VkBufferCreateInfo sbci = { .sType = VK_STRUCTURE_TYPE_BUFFER_CREATE_INFO, .size = si.size_B, .usage = VK_BUFFER_USAGE_TRANSFER_DST_BIT, .sharingMode = VK_SHARING_MODE_EXCLUSIVE };
   for (int k = 0; k < 3; k++) {
      VKOK(vkCreateBuffer(device, &sbci, NULL, &sbuf[k]));
      vkGetBufferMemoryRequirements(device, sbuf[k], &req);
      int st = find_type(&mp, req.memoryTypeBits, VK_MEMORY_PROPERTY_HOST_VISIBLE_BIT | VK_MEMORY_PROPERTY_HOST_COHERENT_BIT, 0);
      if (st < 0) { printf("SNAKE3D FAIL no memory type for the readback buffers\n"); failures++; goto done; }
      mai.allocationSize = req.size;
      mai.memoryTypeIndex = (uint32_t)st;
      VKOK(vkAllocateMemory(device, &mai, NULL, &smem[k]));
      VKOK(vkBindBufferMemory(device, sbuf[k], smem[k], 0));
      VKOK(vkMapMemory(device, smem[k], 0, VK_WHOLE_SIZE, 0, (void **)&smap[k]));
   }
#else
   VkSwapchainCreateInfoKHR sci = { .sType = VK_STRUCTURE_TYPE_SWAPCHAIN_CREATE_INFO_KHR, .surface = surface,
      .minImageCount = caps.minImageCount < 3 ? 3 : caps.minImageCount, .imageFormat = VK_FORMAT_B8G8R8A8_UNORM,
      .imageColorSpace = VK_COLOR_SPACE_SRGB_NONLINEAR_KHR, .imageExtent = { SW, SH }, .imageArrayLayers = 1,
      .imageUsage = VK_IMAGE_USAGE_COLOR_ATTACHMENT_BIT, .imageSharingMode = VK_SHARING_MODE_EXCLUSIVE,
      .preTransform = VK_SURFACE_TRANSFORM_IDENTITY_BIT_KHR, .compositeAlpha = VK_COMPOSITE_ALPHA_OPAQUE_BIT_KHR,
      .presentMode = VK_PRESENT_MODE_FIFO_KHR, .clipped = VK_TRUE, .oldSwapchain = swapchain };
   const VkSwapchainKHR old_swapchain = swapchain;
   VKOK(vkCreateSwapchainKHR(device, &sci, NULL, &swapchain));
   if (old_swapchain != VK_NULL_HANDLE) vkDestroySwapchainKHR(device, old_swapchain, NULL);
   sc_count = MAX_SC_IMAGES;
   VkResult sr = vkGetSwapchainImagesKHR(device, swapchain, &sc_count, sc_images);
   if ((sr != VK_SUCCESS && sr != VK_INCOMPLETE) || sc_count == 0) { printf("SNAKE3D FAIL swapchain images (%d)\n", (int)sr); failures++; goto done; }
   printf("SNAKE3D swapchain of %u images\n", sc_count);
   VkSemaphoreCreateInfo semi = { .sType = VK_STRUCTURE_TYPE_SEMAPHORE_CREATE_INFO };
   VKOK(vkCreateSemaphore(device, &semi, NULL, &acquire_sem));
   for (uint32_t i = 0; i < sc_count; i++) {
      ivci.image = sc_images[i];
      VKOK(vkCreateImageView(device, &ivci, NULL, &sc_views[i]));
      VKOK(vkCreateSemaphore(device, &semi, NULL, &render_sem[i]));
   }
#endif

   VkCommandBufferBeginInfo bbi = { .sType = VK_STRUCTURE_TYPE_COMMAND_BUFFER_BEGIN_INFO };
   VkImageMemoryBarrier b_color = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = image, .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
   VkImageMemoryBarrier b_depth = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .dstAccessMask = VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_WRITE_BIT | VK_ACCESS_DEPTH_STENCIL_ATTACHMENT_READ_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_UNDEFINED, .newLayout = VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = dimage, .subresourceRange = { VK_IMAGE_ASPECT_DEPTH_BIT, 0, 1, 0, 1 } };
#ifdef SNAKE_HOST
   VkImageMemoryBarrier b_src = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT, .dstAccessMask = VK_ACCESS_TRANSFER_READ_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, .newLayout = VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .image = image, .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
#else
   /* what the WSI expects of an image it is given to present: PRESENT_SRC (it moves it to transfer-source itself) */
   VkImageMemoryBarrier b_src = { .sType = VK_STRUCTURE_TYPE_IMAGE_MEMORY_BARRIER, .srcAccessMask = VK_ACCESS_COLOR_ATTACHMENT_WRITE_BIT,
      .oldLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL, .newLayout = VK_IMAGE_LAYOUT_PRESENT_SRC_KHR, .srcQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED,
      .dstQueueFamilyIndex = VK_QUEUE_FAMILY_IGNORED, .subresourceRange = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 1, 0, 1 } };
#endif
   VkRenderingAttachmentInfo ca = { .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO, .imageView = view, .imageLayout = VK_IMAGE_LAYOUT_COLOR_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_STORE, .clearValue = { .color = { .float32 = { 0, 0, 0, 1.0f } } } };
   /* the clear colour is the fog's (snake3d.frag's FOG), which the fragment shader writes through sqrt: the image is UNORM, not sRGB */
   ca.clearValue.color.float32[0] = sqrtf(0.012f); ca.clearValue.color.float32[1] = sqrtf(0.014f); ca.clearValue.color.float32[2] = sqrtf(0.035f);
   VkRenderingAttachmentInfo da = { .sType = VK_STRUCTURE_TYPE_RENDERING_ATTACHMENT_INFO, .imageView = dview, .imageLayout = VK_IMAGE_LAYOUT_DEPTH_STENCIL_ATTACHMENT_OPTIMAL,
      .loadOp = VK_ATTACHMENT_LOAD_OP_CLEAR, .storeOp = VK_ATTACHMENT_STORE_OP_DONT_CARE, .clearValue = { .depthStencil = { 1.0f, 0 } } };
   VkRenderingInfo ri = { .sType = VK_STRUCTURE_TYPE_RENDERING_INFO, .renderArea = { { 0, 0 }, { SW, SH } }, .layerCount = 1, .colorAttachmentCount = 1,
      .pColorAttachments = &ca, .pDepthAttachment = &da };
   VkViewport viewport = { 0, 0, (float)SW, (float)SH, 0.0f, 1.0f };
   VkRect2D scissor = { { 0, 0 }, { SW, SH } };
#ifdef SNAKE_HOST
   /* the buffers' rows are `pitch` bytes apart: bufferRowLength is in texels */
   VkBufferImageCopy region = { .bufferRowLength = si.pitch_B / 4, .bufferImageHeight = SH, .imageSubresource = { VK_IMAGE_ASPECT_COLOR_BIT, 0, 0, 1 }, .imageExtent = { SW, SH, 1 } };
#endif

   while (!quit) {
#ifdef SNAKE_HOST
      double now = frames / 60.0, dt = 1.0 / 60.0;
      if ((int)frames >= host_frames) break;
      if ((int)frames == start_at && G.state == S_TITLE) start_game(now);
#else
      double tn = clock_s(), now = tn - t0, dt = tn - last;
      last = tn;
      if (dt > 0.05) dt = 0.05;
      if (run_seconds > 0 && now >= run_seconds) break;
#endif
      if (have_input && input_poll(&in, now)) break;
#ifndef SNAKE_HOST
      if (windowed) {
         if (gvk_pump(&win, 0) < 0) { printf("SNAKE3D the compositor went away\n"); break; }
         struct gvk_event ev;
         uint32_t nw = SW, nh = SH;
         while (gvk_next_event(&win, &ev)) {
            if (ev.type == GVK_CLOSE) quit = 1;
            else if (ev.type == GVK_KEY && key_apply(ev.code, ev.value, now)) quit = 1;
            else if (ev.type == GVK_RESIZE && ev.value >= 64 && ev.y >= 64) { nw = (uint32_t)ev.value; nh = (uint32_t)ev.y; }
         }
         if (quit) break;
         if (nw != SW || nh != SH) {
            SW = nw; SH = nh;
            resizes++;
            printf("SNAKE3D resized to %ux%u\n", SW, SH);
            goto make_target;
         }
      }
#endif

      /* autopilot: the title plays itself; AUTOPLAY plays for real, and starts again after a game over */
      int ai = G.state == S_TITLE || (autoplay && G.state == S_PLAY);
#ifdef SNAKE_HOST
      if (die_at >= 0 && (int)frames >= die_at) ai = 0;
#endif
      if (autoplay && G.state == S_TITLE && now > 1.5) start_game(now);
      if (autoplay && G.state == S_OVER && now - G.state_at > 2.0) start_game(now);

      float t = 1.0f;
      if (G.state == S_PLAY || G.state == S_TITLE) {
         G.acc += dt * 1000.0;
         while (G.acc >= tick_ms()) {
            G.acc -= tick_ms();
            if (ai) { G.nturns = 0; int d = autopilot(); if (d != G.dir) G.turns[G.nturns++] = d; }
            if (!step(now)) {
               if (G.state == S_TITLE) { G.len = 6; for (int i = 0; i < 6; i++) G.body[i] = (Cell){ 10 - i, GH / 2 }; memcpy(G.prev, G.body, sizeof G.body); G.dir = RIGHT; G.score = 0; break; }
               G.state = S_DYING;
               G.state_at = now;
               G.shake = 6;
               if (G.score > G.best) { G.best = G.score; G.new_best = 1; }
               break;
            }
         }
         if (G.state == S_PLAY || G.state == S_TITLE) t = (float)(G.acc / tick_ms());
         if (G.state == S_TITLE && G.score > 3) { G.score = 0; }
      } else if (G.state == S_DYING && now - G.state_at > 0.65) {
         for (int i = 0; i < G.len; i++) {
            float r, g, b;
            snake_color(i, now, &r, &g, &b);
            burst(cell_world(G.body[i], 0.5f), 10, 3.5f, r, g, b);
         }
         G.shake = 8;
         G.state = S_OVER;
         G.state_at = now;
      }
      parts_update((float)dt);
      if (G.shake > 0) { G.shake -= (float)dt * 8.0f; if (G.shake < 0) G.shake = 0; }

#ifndef SNAKE_HOST
      /* the image to draw into: the WSI never hands out the one on screen */
      uint32_t idx = 0;
      {
         const VkResult ar = vkAcquireNextImageKHR(device, swapchain, UINT64_MAX, acquire_sem, VK_NULL_HANDLE, &idx);
         if (ar != VK_SUCCESS && ar != VK_SUBOPTIMAL_KHR && windowed && win.closed) { printf("SNAKE3D the compositor went away\n"); break; }
         if (ar != VK_SUCCESS && ar != VK_SUBOPTIMAL_KHR) { frame_ok = 0; printf("SNAKE3D acquire failed (%d) at frame %u\n", (int)ar, frames); break; }
      }
      b_color.image = b_src.image = sc_images[idx];
      ca.imageView = sc_views[idx];
#endif

      /* record and submit the frame */
      vkResetCommandBuffer(R.cmd, 0);
      vkBeginCommandBuffer(R.cmd, &bbi);
      VkImageMemoryBarrier first[2] = { b_color, b_depth };
      vkCmdPipelineBarrier(R.cmd, VK_PIPELINE_STAGE_TOP_OF_PIPE_BIT, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT | VK_PIPELINE_STAGE_EARLY_FRAGMENT_TESTS_BIT | VK_PIPELINE_STAGE_LATE_FRAGMENT_TESTS_BIT,
                           0, 0, NULL, 0, NULL, 2, first);
      vkCmdBeginRendering(R.cmd, &ri);
      vkCmdSetViewport(R.cmd, 0, 1, &viewport);
      vkCmdSetScissor(R.cmd, 0, 1, &scissor);
      R.draws = 0;
      draw_scene(now, (float)dt, (float)SW / (float)SH, (float)SW, (float)SH, t);
      if ((unsigned)R.draws > draws_max) draws_max = (unsigned)R.draws;
      vkCmdEndRendering(R.cmd);
      vkCmdPipelineBarrier(R.cmd, VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT, VK_PIPELINE_STAGE_TRANSFER_BIT, 0, 0, NULL, 0, NULL, 1, &b_src);
#ifdef SNAKE_HOST
      int k = frames % 3;
      vkCmdCopyImageToBuffer(R.cmd, image, VK_IMAGE_LAYOUT_TRANSFER_SRC_OPTIMAL, sbuf[k], 1, &region);
#endif
      vkEndCommandBuffer(R.cmd);
      vkResetFences(device, 1, &fence);
      VkSubmitInfo submit = { .sType = VK_STRUCTURE_TYPE_SUBMIT_INFO, .commandBufferCount = 1, .pCommandBuffers = &R.cmd };
#ifndef SNAKE_HOST
      const VkPipelineStageFlags wait_stage = VK_PIPELINE_STAGE_COLOR_ATTACHMENT_OUTPUT_BIT;
      submit.waitSemaphoreCount = 1;
      submit.pWaitSemaphores = &acquire_sem;
      submit.pWaitDstStageMask = &wait_stage;
      submit.signalSemaphoreCount = 1;
      submit.pSignalSemaphores = &render_sem[idx];
#endif
      if (vkQueueSubmit(queue, 1, &submit, fence) != VK_SUCCESS || vkWaitForFences(device, 1, &fence, VK_TRUE, 10000000000ull) != VK_SUCCESS) { frame_ok = 0; break; }

#ifdef SNAKE_HOST
      if (dump_dir && dump_at) {
         char key[16];
         snprintf(key, sizeof key, ",%u,", frames);
         char list[512];
         snprintf(list, sizeof list, ",%s,", dump_at);
         if (strstr(list, key)) {
            char path[512];
            snprintf(path, sizeof path, "%s/frame%05u.ppm", dump_dir, frames);
            FILE *f = fopen(path, "wb");
            if (f) {
               fprintf(f, "P6\n%u %u\n255\n", SW, SH);
               for (uint32_t y = 0; y < SH; y++)
                  for (uint32_t x = 0; x < SW; x++) {
                     const uint8_t *px = smap[k] + (size_t)y * si.pitch_B + x * 4;   /* B8G8R8A8 */
                     uint8_t rgb[3] = { px[2], px[1], px[0] };
                     fwrite(rgb, 1, 3, f);
                  }
               fclose(f);
               printf("SNAKE3D wrote %s (state %d, score %u, len %d, %d draws)\n", path, G.state, G.score, G.len, R.draws);
            }
         }
      }
      shown++;
#else
      /* the WSI copies the image into a scanout buffer on the GPU, waits for the previous flip and points the display at the buffer */
      VkPresentInfoKHR pinfo = { .sType = VK_STRUCTURE_TYPE_PRESENT_INFO_KHR, .waitSemaphoreCount = 1, .pWaitSemaphores = &render_sem[idx],
         .swapchainCount = 1, .pSwapchains = &swapchain, .pImageIndices = &idx };
      const VkResult pr = vkQueuePresentKHR(queue, &pinfo);
      /* the compositor ended (its session, Ctrl+Alt+Backspace): the connection is gone and so is the window; that is an ending, not a failure */
      if (pr != VK_SUCCESS && pr != VK_SUBOPTIMAL_KHR && windowed && win.closed) { printf("SNAKE3D the compositor went away\n"); break; }
      if (pr != VK_SUCCESS && pr != VK_SUBOPTIMAL_KHR) { present_ok = 0; printf("SNAKE3D present failed (%d) at frame %u\n", (int)pr, frames); break; }
      shown++;
#endif
      frames++;
   }
   {
      double el = clock_s() - t0;
#ifdef SNAKE_HOST
      el = frames / 60.0;
#endif
      if (!frame_ok) { failures++; printf("SNAKE3D FAIL a frame was not rendered and fenced\n"); }
      if (!present_ok || shown != frames) { failures++; printf("SNAKE3D FAIL %u of %u frames were put on the screen\n", shown, frames); }
      printf("SNAKE3D %u frames in %.1f s (%.1f per second), score %u best %u, up to %u draws per frame\n",
             frames, el, frames / (el > 0 ? el : 1), G.score, G.best, draws_max);
   }
#ifndef SNAKE_HOST
   if (windowed)
      printf("SNAKE3D window: %u resizes, %u buffers sent, %u commits, %u releases, %u buffers destroyed so far, %u throttled by the compositor (%u timed out)\n",
             resizes, win.buffers_sent, win.commits, win.releases, win.buffers_destroyed, win.throttled, win.throttle_timeouts);
#endif
   VKOK(vkDeviceWaitIdle(device));

done:
   if (have_input) input_close(&in);
   if (device) {
      PFN_vkDeviceWaitIdle wi = (PFN_vkDeviceWaitIdle)vkGetDeviceProcAddr(device, "vkDeviceWaitIdle");
      if (wi) wi(device);
#define GONE(handle, fn) do { PFN_##fn f_ = (PFN_##fn)vkGetDeviceProcAddr(device, #fn); if (handle && f_) f_(device, handle, NULL); } while (0)
      for (int p = 0; p < 3; p++) GONE(R.pipe[p], vkDestroyPipeline);
      GONE(pl, vkDestroyPipelineLayout);
      GONE(fmod, vkDestroyShaderModule);
      GONE(vmod, vkDestroyShaderModule);
#ifdef SNAKE_HOST
      for (int k = 0; k < 3; k++) { GONE(sbuf[k], vkDestroyBuffer); GONE(smem[k], vkFreeMemory); }
#else
      for (uint32_t i = 0; i < MAX_SC_IMAGES; i++) { GONE(render_sem[i], vkDestroySemaphore); GONE(sc_views[i], vkDestroyImageView); }
      GONE(acquire_sem, vkDestroySemaphore);
      GONE(swapchain, vkDestroySwapchainKHR);
#endif
      GONE(dview, vkDestroyImageView);
      GONE(dimage, vkDestroyImage);
      GONE(dmem, vkFreeMemory);
      GONE(view, vkDestroyImageView);
      GONE(image, vkDestroyImage);
      GONE(imem, vkFreeMemory);
      GONE(fence, vkDestroyFence);
      GONE(cpool, vkDestroyCommandPool);
      PFN_vkDestroyDevice dd = (PFN_vkDestroyDevice)vkGetDeviceProcAddr(device, "vkDestroyDevice");
      if (dd) dd(device, NULL);
   }
#ifndef SNAKE_HOST
   if (instance && surface) {
      PFN_vkDestroySurfaceKHR ds = (PFN_vkDestroySurfaceKHR)GET_INSTANCE_PROC(instance, "vkDestroySurfaceKHR");
      if (ds) ds(instance, surface, NULL);
   }
   if (have_window) gvk_close(&win);
#endif
   if (instance) {
      PFN_vkDestroyInstance di = (PFN_vkDestroyInstance)GET_INSTANCE_PROC(instance, "vkDestroyInstance");
      if (di) di(instance, NULL);
   }
   if (failures) { printf("SNAKE3D FAILED (%d)\n", failures); return 1; }
   printf("SNAKE3D DONE\n");
   return 0;
}
