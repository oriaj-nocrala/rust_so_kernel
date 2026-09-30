#version 450
layout(push_constant) uniform PC { float angle; float aspect; vec2 pad; vec4 color; } pc;
layout(location = 0) out vec4 color;
void main() {
    color = pc.color;
}
