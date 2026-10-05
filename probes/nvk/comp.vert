#version 450
// One screen-aligned rectangle per draw, from gl_VertexIndex and the push constants (comp.frag has the same block).
layout(push_constant) uniform PC {
    ivec4 dst;      // x, y, w, h on screen: what the draw covers
    ivec4 src;      // x, y inside the source, stride in pixels, mode (0 solid, 1 buffer, 2 buffer with a key, 3 shape, 4 premultiplied buffer)
    uvec4 misc;     // colour, screen w, screen h, flags (shape: bit 0 horizontal gradient, bit 1 glass; glass: its region's height << 8)
    ivec4 box;      // shape: its box on screen (dst also covers the shadow)
    vec4 geom;      // shape: corner radius, border width, gradient split, shadow blur
    uvec4 grad;     // shape: the gradient's four colours, 0xAARRGGBB straight alpha
    uvec4 extra;    // shape: border colour, shadow colour, shadow dx, shadow dy (ints)
} pc;

void main() {
    const vec2 corner[6] = vec2[6](vec2(0, 0), vec2(1, 0), vec2(0, 1), vec2(1, 0), vec2(1, 1), vec2(0, 1));
    vec2 p = vec2(pc.dst.xy) + corner[gl_VertexIndex] * vec2(pc.dst.zw);
    vec2 screen = vec2(pc.misc.yz);
    gl_Position = vec4(p / screen * 2.0 - 1.0, 0.0, 1.0);
}
