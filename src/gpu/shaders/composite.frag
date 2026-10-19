#version 450
// The 2D layer over the GPU scene: the software overlays were drawn twice,
// once over a black scene (`black`) and once over a white one (`white`). Every
// overlay operation is affine in the destination, so the final pixel is
// `black + scene * (white - black)`.

layout(location = 0) in vec2 in_uv;
layout(location = 0) out vec4 out_colour;

layout(set = 2, binding = 0) uniform sampler2D black;
layout(set = 2, binding = 1) uniform sampler2D white;
layout(set = 2, binding = 2) uniform sampler2D scene;

void main() {
    vec3 b = texture(black, in_uv).rgb;
    vec3 w = texture(white, in_uv).rgb;
    vec3 s = texture(scene, in_uv).rgb;
    out_colour = vec4(clamp(b + s * (w - b), 0.0, 1.0), 1.0);
}
