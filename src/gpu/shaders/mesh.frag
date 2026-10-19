#version 450
// Indexed TIM sampling through the primitive's CLUT row. The palette's alpha
// carries the STP bit; a semi-transparent packet blends only its STP texels.

layout(location = 0) in vec2 in_uv;
layout(location = 1) noperspective in vec3 in_shade;
layout(location = 2) flat in uint in_packed;

layout(location = 0) out vec4 out_colour;

layout(set = 2, binding = 0) uniform usampler2D indices;
layout(set = 2, binding = 1) uniform sampler2D palette;

layout(std140, set = 3, binding = 0) uniform Material {
    vec4 params; // x = background weight in 0..1, y = pass (0 opaque, 1 blend)
};

void main() {
    bool untextured = ((in_packed >> 25) & 1u) != 0u;
    bool packet_blend = ((in_packed >> 24) & 1u) != 0u;
    vec3 texel = vec3(255.0);
    bool stp = false;
    if (!untextured) {
        ivec2 size = textureSize(indices, 0);
        ivec2 coord = ivec2(mod(floor(in_uv), vec2(size)));
        uint index = texelFetch(indices, coord, 0).r;
        if (index == 0u) {
            discard;
        }
        int row = int((in_packed >> 8) & 0xFFFFu);
        vec4 entry = vec4(0.0);
        if (row < textureSize(palette, 0).y) {
            entry = texelFetch(palette, ivec2(int(index), row), 0);
        }
        texel = entry.rgb * 255.0;
        stp = entry.a > 0.5;
    }
    bool blend = packet_blend && stp;
    if ((params.y < 0.5) == blend) {
        discard;
    }
    vec3 rgb = clamp(floor(texel * in_shade / 255.0 + 0.5), 0.0, 255.0) / 255.0;
    out_colour = vec4(rgb, blend ? 1.0 - params.x : 1.0);
}
