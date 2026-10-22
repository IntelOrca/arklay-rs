#version 450
// A model the CPU has already projected (`htexture::Model`): the examine
// viewer's item, drawn into its own target with `mesh.frag`. Positions arrive
// in clip space over the target and the colour already lit.

layout(location = 0) in vec4 in_clip;
layout(location = 1) in vec2 in_uv;       // texel coordinates
layout(location = 2) in vec3 in_shade;    // 0..255, 255 = the texel as stored
layout(location = 3) in uint in_packed;   // row << 8 | blend << 24 | untextured << 25

layout(location = 0) out vec2 out_uv;
layout(location = 1) noperspective out vec3 out_shade;
layout(location = 2) flat out uint out_packed;

void main() {
    gl_Position = in_clip;
    out_uv = in_uv;
    out_shade = in_shade;
    out_packed = in_packed;
}
