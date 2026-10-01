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
    ivec2 p = ivec2(gl_FragCoord.xy) - pc.dst.xy + pc.src.xy;
    uint v = pix.px[uint(p.y * pc.src.z + p.x)];
    if (pc.src.w == 2 && (v >> 24) == 0u) discard;
    color = unpack(v);
}
