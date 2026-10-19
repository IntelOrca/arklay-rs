#version 450
// Screen-space quads (background, masks, shadows, effect billboards). The CPU
// supplies clip-space positions so shadows keep perspective-correct UVs.

layout(location = 0) in vec4 in_clip;
layout(location = 1) in vec2 in_uv;
layout(location = 2) in vec4 in_colour;
layout(location = 3) in uint in_mode;

layout(location = 0) out vec2 out_uv;
layout(location = 1) flat out vec4 out_colour;
layout(location = 2) flat out uint out_mode;

void main() {
    gl_Position = in_clip;
    out_uv = in_uv;
    out_colour = in_colour;
    out_mode = in_mode;
}
