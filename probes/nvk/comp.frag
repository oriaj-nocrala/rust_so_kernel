#version 450
// Shows a solid colour or the pixel of a client's buffer under it, one for one: no sampler, no format, no filtering. A buffer holds
// 0x00RRGGBB words; mode 2 treats a word whose top byte is 0 as transparent (the cursor).
layout(push_constant) uniform PC {
    ivec4 dst;
    ivec4 src;
    uvec4 misc;
} pc;
layout(std430, set = 0, binding = 0) readonly buffer Pix { uint px[]; } pix;
layout(location = 0) out vec4 color;

vec4 unpack(uint v) {
    return vec4(float((v >> 16) & 255u), float((v >> 8) & 255u), float(v & 255u), 255.0) / 255.0;
}

void main() {
    if (pc.src.w == 0) {
        color = unpack(pc.misc.x);
        return;
    }
    // Helper invocations (the 2x2 quads that straddle the rectangle's edge when it starts on an odd pixel) run the loads too, with FragCoord
    // outside the rectangle: an index of -1 is uint 0xFFFFFFFF, 16 GiB past the buffer, in another session's address space, and RM resets
    // the channel (Ryzen #184-#187: the first time the pointer moved by one pixel). Their values are never shown, so clamp into the rectangle.
    ivec2 rel = clamp(ivec2(gl_FragCoord.xy) - pc.dst.xy, ivec2(0), max(pc.dst.zw - 1, ivec2(0)));
    ivec2 p = rel + pc.src.xy;
    uint v = pix.px[uint(p.y * pc.src.z + p.x)];
    if (pc.src.w == 2 && (v >> 24) == 0u) discard;
    color = unpack(v);
}
