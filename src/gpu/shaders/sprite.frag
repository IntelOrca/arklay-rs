#version 450
// Modes: 0 = textured (rgb * colour, alpha = colour.a), 1 = shadow (colour
// with the texel alpha as coverage), 2 = clamped background (opaque). Texture
// coordinates are texels for modes 0 and 2 and normalised for mode 1. An HD
// replacement (htexture) is bound in place of the original with `scale` set to
// its texels per original texel; it is (1, 1) for the original.

layout(location = 0) in vec2 in_uv;
layout(location = 1) flat in vec4 in_colour;
layout(location = 2) flat in uint in_mode;

layout(location = 0) out vec4 out_colour;

layout(set = 2, binding = 0) uniform sampler2D page;

layout(std140, set = 3, binding = 0) uniform Replacement {
    vec4 scale; // xy = texels per original texel
};

void main() {
    ivec2 size = textureSize(page, 0);
    if (in_mode == 1u) {
        ivec2 coord = ivec2(mod(floor(in_uv * vec2(size)), vec2(size)));
        float alpha = texelFetch(page, coord, 0).a;
        if (alpha == 0.0) {
            discard;
        }
        out_colour = vec4(in_colour.rgb, alpha);
        return;
    }
    ivec2 coord = clamp(ivec2(floor(in_uv * scale.xy)), ivec2(0), size - 1);
    vec4 texel = texelFetch(page, coord, 0);
    if (in_mode == 2u) {
        out_colour = vec4(texel.rgb * in_colour.rgb, 1.0);
        return;
    }
    // An original texel is clear or opaque; a replacement's soft edge keeps
    // its nearer half, as masks write depth.
    if (texel.a < 0.5) {
        discard;
    }
    out_colour = vec4(texel.rgb * in_colour.rgb, in_colour.a);
}
