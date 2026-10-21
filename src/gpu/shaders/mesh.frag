#version 450
// Indexed TIM sampling through the primitive's CLUT row. The palette's alpha
// carries the STP bit; a semi-transparent packet blends only its STP texels.
// With an HD replacement (htexture) the colour comes from its layer for the
// CLUT row while the original index still decides transparency and STP.

layout(location = 0) in vec2 in_uv;
layout(location = 1) noperspective in vec3 in_shade;
layout(location = 2) flat in uint in_packed;

layout(location = 0) out vec4 out_colour;

layout(set = 2, binding = 0) uniform usampler2D indices;
layout(set = 2, binding = 1) uniform sampler2D palette;
// One layer per replaced CLUT row, each in its top-left corner.
layout(set = 2, binding = 2) uniform sampler2DArray hd;
// Per CLUT row: (layer + 1 or 0, original texels covered x, y, 0) at x = 0
// and (replacement width, height, 0, 0) at x = 1.
layout(set = 2, binding = 3) uniform usampler2D hd_rows;

layout(std140, set = 3, binding = 0) uniform Material {
    // x = background weight in 0..1, y = pass (0 opaque, 1 blend),
    // z = 1 when `hd` and `hd_rows` hold this page's replacements
    vec4 params;
};

void main() {
    bool untextured = ((in_packed >> 25) & 1u) != 0u;
    bool packet_blend = ((in_packed >> 24) & 1u) != 0u;
    vec3 texel = vec3(255.0);
    bool stp = false;
    if (!untextured) {
        ivec2 size = textureSize(indices, 0);
        vec2 source = mod(in_uv, vec2(size));
        ivec2 coord = min(ivec2(floor(source)), size - 1);
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
        if (params.z > 0.5 && row < textureSize(hd_rows, 0).y) {
            uvec4 place = texelFetch(hd_rows, ivec2(0, row), 0);
            if (place.x != 0u) {
                ivec2 extent = ivec2(texelFetch(hd_rows, ivec2(1, row), 0).xy);
                vec2 at = floor(source * vec2(extent) / vec2(place.yz));
                ivec2 hd_coord = clamp(ivec2(at), ivec2(0), extent - 1);
                texel = texelFetch(hd, ivec3(hd_coord, int(place.x) - 1), 0).rgb * 255.0;
            }
        }
    }
    bool blend = packet_blend && stp;
    if ((params.y < 0.5) == blend) {
        discard;
    }
    vec3 rgb = clamp(floor(texel * in_shade / 255.0 + 0.5), 0.0, 255.0) / 255.0;
    out_colour = vec4(rgb, blend ? 1.0 - params.x : 1.0);
}
