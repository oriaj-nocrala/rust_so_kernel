#version 450
// snake3d (vk_snake.c): materials, in b.w: 0 lit solid, 1 floor (checker inside the arena, a grid outside), 2 additive glow, 3 blob shadow,
// 4 HUD glyph. Colours arrive as sRGB and leave as sRGB: the lighting is done on their squares.
layout(push_constant) uniform PC {
    mat4 vp;
    vec4 a;
    vec4 b;
    vec4 c;
    vec4 d;
    vec4 e;
    uvec4 g;
} pc;

layout(location = 0) in vec3 wpos;
layout(location = 1) in vec3 nrm;
layout(location = 2) in vec2 uv;
layout(location = 0) out vec4 outColor;

const vec3 FOG = vec3(0.012, 0.014, 0.035);       // also the clear colour
const vec3 FOOD = vec3(1.0, 0.24, 0.5);
const vec2 ARENA = vec2(24.0, 14.0);                // GW, GH of vk_snake.c

vec3 lin(vec3 c) { return c * c; }
vec3 srgb(vec3 c) { return sqrt(max(c, vec3(0.0))); }

vec3 shade(vec3 base, vec3 N, vec3 V, float emissive, float shininess) {
    vec3 L = normalize(vec3(-0.35, 1.0, 0.45));
    float diff = max(dot(N, L), 0.0);
    float hemi = 0.35 + 0.25 * N.y;
    vec3 toFood = pc.e.xyz - wpos;
    float fd2 = dot(toFood, toFood);
    float fdiff = max(dot(N, toFood * inversesqrt(fd2 + 1e-4)), 0.0) * pc.e.w / (1.0 + 0.25 * fd2);
    float spec = pow(max(dot(N, normalize(L + V)), 0.0), shininess);
    float rim = pow(1.0 - max(dot(N, V), 0.0), 3.0);
    vec3 col = base * (hemi + 0.85 * diff) + lin(FOOD) * base * fdiff * 2.2;
    col += vec3(spec) * 0.55 + base * rim * 0.5 + base * emissive;
    return col;
}

void main() {
    int mat = int(pc.b.w + 0.5);
    vec3 V = normalize(pc.d.xyz - wpos);
    vec3 N = normalize(nrm);
    float dist = length(pc.d.xyz - wpos);
    float fog = 1.0 - exp(-dist * dist * 0.0009);
    vec3 col;
    if (mat == 0) {
        col = shade(lin(pc.c.rgb), N, V, pc.c.a, 60.0);
        col = mix(col, FOG, fog);
    } else if (mat == 1) {
        vec2 p = wpos.xz;
        bool inside = p.x >= 0.0 && p.y >= 0.0 && p.x < ARENA.x && p.y < ARENA.y;
        vec3 base;
        if (inside) {
            float chk = mod(floor(p.x) + floor(p.y), 2.0);
            base = mix(vec3(0.030, 0.034, 0.075), vec3(0.052, 0.050, 0.105), chk);
            vec2 f = abs(fract(p) - 0.5);
            float edge = smoothstep(0.46, 0.5, max(f.x, f.y));
            base += vec3(0.05, 0.08, 0.18) * edge;
        } else {
            vec2 f = abs(fract(p / 2.0) - 0.5);
            float line = smoothstep(0.47, 0.5, max(f.x, f.y));
            base = vec3(0.012, 0.014, 0.03) + vec3(0.10, 0.04, 0.22) * line;
        }
        col = shade(base, vec3(0.0, 1.0, 0.0), V, 0.0, 30.0) - vec3(0.0);
        col -= vec3(pow(max(dot(vec3(0.0, 1.0, 0.0), normalize(vec3(-0.35, 1.0, 0.45) + V)), 0.0), 30.0)) * 0.4;
        col = mix(col, FOG, fog);
    } else if (mat == 2) {
        float k = pow(abs(dot(N, V)), 2.0);
        col = lin(pc.c.rgb) * k * pc.c.a;
    } else if (mat == 3) {
        vec2 q = (wpos.xz - pc.a.xz) / pc.b.xz;
        float a = pc.c.a * (1.0 - smoothstep(0.15, 1.0, length(q)));
        outColor = vec4(0.0, 0.0, 0.0, a);
        return;
    } else {
        int gx = min(int(uv.x * 5.0), 4), gy = min(int(uv.y * 7.0), 6);
        int bit = gy * 5 + gx;
        uint word = bit < 30 ? pc.g.x : pc.g.y;
        if (((word >> uint(bit < 30 ? bit : bit - 30)) & 1u) == 0u) discard;
        outColor = vec4(pc.c.rgb, 1.0);
        return;
    }
    outColor = vec4(srgb(col), 1.0);
}
