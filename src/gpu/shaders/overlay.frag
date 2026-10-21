#version 450
// The scene under the 2D overlay with the frame's HD replacements of 2D art
// drawn over it (`htexture::Overlay`). Mode 1 copies the gameplay scene;
// mode `2 + k` is the frame's quad `k`, filtered, times the draw's colour and
// blended by its own alpha, kept only where the coverage mask says quad `k`
// wrote the original texel last, so HD art never shows outside its
// original's coverage. The composite shows this target where the overlay
// left a hole for it.

layout(location = 0) in vec2 in_uv;
layout(location = 1) flat in vec4 in_colour;
layout(location = 2) flat in uint in_mode;

layout(location = 0) out vec4 out_colour;

layout(set = 2, binding = 0) uniform sampler2D image;
// Per frame pixel: one more than the index of the quad that last wrote it,
// or 0.
layout(set = 2, binding = 1) uniform usampler2D coverage;

layout(set = 3, binding = 0) uniform Overlay {
    // Coverage texels per target pixel.
    vec2 coverage_scale;
};

void main() {
    vec4 texel = texture(image, in_uv);
    if (in_mode == 1u) {
        out_colour = vec4(texel.rgb, 1.0);
        return;
    }
    ivec2 size = textureSize(coverage, 0);
    ivec2 at = min(ivec2(gl_FragCoord.xy * coverage_scale), size - 1);
    if (texelFetch(coverage, at, 0).r != in_mode - 1u) {
        discard;
    }
    out_colour = vec4(texel.rgb * in_colour.rgb, texel.a * in_colour.a);
}
