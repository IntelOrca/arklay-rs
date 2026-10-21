#version 450
// The scene under the 2D overlay with the frame's HD replacements of 2D art
// drawn over it (`htexture::Overlay`). Mode 1 copies the gameplay scene;
// mode 0 is a replacement, filtered, times the draw's colour and blended by
// its own alpha. The composite shows this target where the overlay left a
// hole for it.

layout(location = 0) in vec2 in_uv;
layout(location = 1) flat in vec4 in_colour;
layout(location = 2) flat in uint in_mode;

layout(location = 0) out vec4 out_colour;

layout(set = 2, binding = 0) uniform sampler2D image;

void main() {
    vec4 texel = texture(image, in_uv);
    if (in_mode == 1u) {
        out_colour = vec4(texel.rgb, 1.0);
        return;
    }
    out_colour = vec4(texel.rgb * in_colour.rgb, texel.a * in_colour.a);
}
