#version 450
// A spinning triangle: G4e's presentation demo (vk_draw.c). Push constants: angle, aspect ratio, padding, colour.
layout(push_constant) uniform PC { float angle; float aspect; vec2 pad; vec4 color; } pc;
void main() {
    vec2 v[3] = vec2[](vec2(0.0, 0.8), vec2(-0.7, -0.6), vec2(0.7, -0.6));
    vec2 p = v[gl_VertexIndex];
    float c = cos(pc.angle), s = sin(pc.angle);
    p = vec2(c * p.x - s * p.y, s * p.x + c * p.y);
    p.x /= pc.aspect;
    // Vulkan clip space has +y pointing DOWN the screen: flip it so the triangle stands on its base
    gl_Position = vec4(p.x, -p.y, 0.0, 1.0);
}
