#version 450
// snake3d (vk_snake.c): every object is one draw with no vertex buffer: the vertices are made here from gl_VertexIndex and the object's
// push constants. kind 0 = ellipsoid (UV sphere), 1 = box, 2 = screen-space quad (HUD glyphs; vp is then the pixel -> clip matrix).
layout(push_constant) uniform PC {
    mat4 vp;
    vec4 a;    // xyz centre (quad: x, y = top left in pixels, z = depth layer), w kind
    vec4 b;    // xyz half extents (quad: x, y = size in pixels), w material
    vec4 c;    // rgb colour, w emissive / opacity
    vec4 d;    // xyz eye, w time in seconds
    vec4 e;    // xyz food light position, w its intensity
    uvec4 g;   // glyph bits (quad)
} pc;

layout(location = 0) out vec3 wpos;
layout(location = 1) out vec3 nrm;
layout(location = 2) out vec2 uv;

const int LON = 20;
const int LAT = 12;
const float PI = 3.14159265;

void main() {
    // the six corners of quad cell i/6 as two triangles
    const vec2 corner[6] = vec2[](vec2(0, 0), vec2(1, 0), vec2(0, 1), vec2(1, 0), vec2(1, 1), vec2(0, 1));
    int kind = int(pc.a.w + 0.5);
    int i = gl_VertexIndex;
    int cell = i / 6;
    vec2 cn = corner[i % 6];
    if (kind == 0) {
        float u = (float(cell % LON) + cn.x) / float(LON) * 2.0 * PI;
        float v = (float(cell / LON) + cn.y) / float(LAT) * PI;
        vec3 p = vec3(sin(v) * cos(u), cos(v), sin(v) * sin(u));
        wpos = pc.a.xyz + p * pc.b.xyz;
        nrm = normalize(p / pc.b.xyz);
        uv = cn;
    } else if (kind == 1) {
        int axis = cell / 2;
        float sgn = (cell % 2 == 0) ? 1.0 : -1.0;
        vec3 p = vec3(0.0);
        p[axis] = sgn;
        p[(axis + 1) % 3] = cn.x * 2.0 - 1.0;
        p[(axis + 2) % 3] = cn.y * 2.0 - 1.0;
        vec3 n = vec3(0.0);
        n[axis] = sgn;
        wpos = pc.a.xyz + p * pc.b.xyz;
        nrm = n;
        uv = cn;
    } else {
        wpos = vec3(pc.a.xy + cn * pc.b.xy, pc.a.z);
        nrm = vec3(0.0, 0.0, 1.0);
        uv = cn;
    }
    gl_Position = pc.vp * vec4(wpos, 1.0);
}
