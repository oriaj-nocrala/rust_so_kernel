#version 450
// The colour of one pixel of a draw, premultiplied; the pipeline blends it "over" what is under it (ONE, ONE_MINUS_SRC_ALPHA), so an opaque
// one (alpha 1) replaces it exactly. Modes: a solid colour; a client's buffer one for one (no sampler, no format, no filtering), its words
// 0x00RRGGBB, with a colour key (2: top byte 0 = transparent, the cursor) or premultiplied 0xAARRGGBB (4); a shape (3): a rounded box with a
// gradient, a border and a soft shadow, all from its signed distance, so edges are anti-aliased; glass (misc.w bit 1): its fill over a
// blurred copy of what is behind it, the bound buffer (blur.comp's output, the region at src.xy, src.z wide, misc.w >> 8 tall).
// host_comp.c's reference does the same maths.
layout(push_constant) uniform PC {
    ivec4 dst;
    ivec4 src;
    uvec4 misc;
    ivec4 box;
    vec4 geom;
    uvec4 grad;
    uvec4 extra;
} pc;
layout(std430, set = 0, binding = 0) readonly buffer Pix { uint px[]; } pix;
layout(location = 0) out vec4 color;

vec4 unpack(uint v) {
    return vec4(float((v >> 16) & 255u), float((v >> 8) & 255u), float(v & 255u), 255.0) / 255.0;
}

// 0xAARRGGBB, alpha straight -> premultiplied
vec4 unpack_straight(uint v) {
    vec4 c = vec4(float((v >> 16) & 255u), float((v >> 8) & 255u), float(v & 255u), float(v >> 24)) / 255.0;
    return vec4(c.rgb * c.a, c.a);
}

// Distance from p to a box of half-size `b` centred on 0 with corners of radius r: negative inside.
float sd_round_box(vec2 p, vec2 b, float r) {
    vec2 q = abs(p) - b + r;
    return length(max(q, 0.0)) + min(max(q.x, q.y), 0.0) - r;
}

vec4 shape() {
    vec2 p = gl_FragCoord.xy;   // the pixel's centre
    vec2 b = vec2(pc.box.zw) * 0.5;
    vec2 centre = vec2(pc.box.xy) + b;
    float r = min(pc.geom.x, min(b.x, b.y));
    float d = sd_round_box(p - centre, b, r);
    float cover = clamp(0.5 - d, 0.0, 1.0);
    float t = (pc.misc.w & 1u) != 0u ? (p.x - float(pc.box.x)) / float(pc.box.z) : (p.y - float(pc.box.y)) / float(pc.box.w);
    t = clamp(t, 0.0, 1.0);
    float split = pc.geom.z;
    vec4 fill = t < split ? mix(unpack_straight(pc.grad.x), unpack_straight(pc.grad.y), t / split)
                          : mix(unpack_straight(pc.grad.z), unpack_straight(pc.grad.w), split < 1.0 ? (t - split) / (1.0 - split) : 1.0);
    float bw = pc.geom.y;
    if (bw > 0.0) fill = mix(unpack_straight(pc.extra.x), fill, clamp(0.5 - (d + bw), 0.0, 1.0));
    if ((pc.misc.w & 2u) != 0u) {
        // the blurred backdrop under the fill (opaque): clamped into the region, as the buffer loads of the other modes are
        ivec2 q = clamp(ivec2(p) - pc.src.xy, ivec2(0), ivec2(pc.src.z - 1, int(pc.misc.w >> 8) - 1));
        vec4 back = unpack(pix.px[uint(q.y * pc.src.z + q.x)]);
        fill += back * (1.0 - fill.a);
    }
    vec4 o = fill * cover;
    vec4 sc = unpack_straight(pc.extra.y);
    if (sc.a > 0.0) {
        float blur = pc.geom.w;
        float ds = sd_round_box(p - centre - vec2(ivec2(pc.extra.zw)), b, r);
        float s = blur > 0.0 ? 1.0 - smoothstep(-blur, blur, ds) : clamp(0.5 - ds, 0.0, 1.0);
        o += sc * (s * (1.0 - cover));   // the shadow is only around the shape, never seen through it
    }
    return o;
}

void main() {
    uint mode = uint(pc.src.w);
    if (mode == 0u) {
        color = unpack(pc.misc.x);
        return;
    }
    if (mode == 3u) {
        color = shape();
        return;
    }
    // Helper invocations (the 2x2 quads that straddle the rectangle's edge when it starts on an odd pixel) run the loads too, with FragCoord
    // outside the rectangle: an index of -1 is uint 0xFFFFFFFF, 16 GiB past the buffer, in another session's address space, and RM resets
    // the channel (Ryzen #184-#187: the first time the pointer moved by one pixel). Their values are never shown, so clamp into the rectangle.
    ivec2 rel = clamp(ivec2(gl_FragCoord.xy) - pc.dst.xy, ivec2(0), max(pc.dst.zw - 1, ivec2(0)));
    ivec2 p = rel + pc.src.xy;
    uint v = pix.px[uint(p.y * pc.src.z + p.x)];
    if (mode == 4u) {
        color = vec4(float((v >> 16) & 255u), float((v >> 8) & 255u), float(v & 255u), float(v >> 24)) / 255.0;
        return;
    }
    if (mode == 2u && (v >> 24) == 0u) discard;
    color = unpack(v);
}
