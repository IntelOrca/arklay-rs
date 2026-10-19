#version 450
// The 2D layer over the GPU scene: the software overlays were drawn twice,
// once over a black scene (`black`) and once over a white one (`white`). Every
// overlay operation is affine in the destination, so the final pixel is
// `black + scene * (white - black)`.
//
// The 320x240 layers are scaled to the viewport sharply: each screen pixel
// takes its texel's colour, and only a pixel straddling two texels blends
// them by its coverage. At a whole zoom no pixel straddles, so the result is
// exactly nearest sampling; at a fractional zoom every texel still covers the
// same width. The samplers are bilinear, which also filters the scene target
// into the viewport when the two sizes differ.

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_colour;

layout(set = 2, binding = 0) uniform sampler2D black;
layout(set = 2, binding = 1) uniform sampler2D white;
layout(set = 2, binding = 2) uniform sampler2D scene;

void main() {
    vec2 size = vec2(textureSize(black, 0));
    vec2 texel = in_uv * size;
    // Screen pixels per texel along each axis.
    vec2 zoom = 1.0 / max(fwidth(texel), vec2(1.0e-6));
    vec2 offset = fract(texel) - 0.5;
    vec2 inner = 0.5 - 0.5 / zoom;
    vec2 blend = (offset - clamp(offset, -inner, inner)) * zoom + 0.5;
    vec2 uv = (floor(texel) + blend) / size;

    vec3 b = texture(black, uv).rgb;
    vec3 w = texture(white, uv).rgb;
    vec3 s = texture(scene, in_uv).rgb;
    out_colour = vec4(clamp(b + s * (w - b), 0.0, 1.0), 1.0);
}
