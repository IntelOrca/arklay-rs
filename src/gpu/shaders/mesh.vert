#version 450
// Rigid-skin TMD vertex shader: one 4.12 joint matrix per mesh object, the
// reference's per-vertex room lighting, then the cut camera's projection.
// Everything per frame arrives in the uniform block; the vertex buffer is the
// mesh's static geometry, uploaded once (see `src/gpu/scene.rs`).

layout(location = 0) in vec3 in_position;  // stored vertex, input Y already conjugated
layout(location = 1) in vec3 in_normal;    // stored normal with Y negated
layout(location = 2) in vec2 in_uv;        // texel coordinates with the page offset
layout(location = 3) in vec3 in_colour;    // untextured packet colour / 1024, else 1
layout(location = 4) in uint in_packed;    // joint | row << 8 | blend << 24 | untextured << 25

layout(location = 0) out vec2 out_uv;
layout(location = 1) noperspective out vec3 out_shade;
layout(location = 2) flat out uint out_packed;

layout(std140, set = 1, binding = 0) uniform Draw {
    vec4 view[3];       // xyz = view row / 4096, w = translation
    vec4 projection;    // fov, shake x, shake y, near
    vec4 depth;         // x = far plane
    vec4 ambient;       // rgb in 0..255
    vec4 light_colour[3]; // rgb * 255 / 128
    vec4 tint;          // rgb in 0..255
    uvec4 flags;        // x = hidden joints (bit per joint)
    vec4 joints[32 * 6]; // per joint: 3 world rows (xyz, w = t), then 3 light vectors
};

void main() {
    uint joint = in_packed & 0xFFu;
    if (joint >= 32u || ((flags.x >> joint) & 1u) != 0u) {
        gl_Position = vec4(0.0, 0.0, 0.0, 1.0);
        out_uv = vec2(0.0);
        out_shade = vec3(0.0);
        out_packed = in_packed;
        return;
    }
    uint base = joint * 6u;
    vec3 world = vec3(
        dot(joints[base + 0u].xyz, in_position) + joints[base + 0u].w,
        dot(joints[base + 1u].xyz, in_position) + joints[base + 1u].w,
        dot(joints[base + 2u].xyz, in_position) + joints[base + 2u].w);
    vec3 v = vec3(
        dot(view[0].xyz, world) + view[0].w,
        dot(view[1].xyz, world) + view[1].w,
        dot(view[2].xyz, world) + view[2].w);
    float fov = projection.x;
    float near = projection.w;
    float far = depth.x;
    // sx = vx*fov/vz + 160 + shake_x and sy = 120 + shake_y - vy*fov/vz,
    // expressed in clip space with w = vz.
    gl_Position = vec4(
        (v.x * fov + projection.y * v.z) / 160.0,
        (v.y * fov - projection.z * v.z) / 120.0,
        far * (v.z - near) / (far - near),
        v.z);

    bool blend = ((in_packed >> 24) & 1u) != 0u;
    vec3 shade;
    if (blend) {
        // A semi-transparent packet is full-bright: the mesh tint alone.
        shade = tint.rgb;
    } else {
        shade = ambient.rgb;
        for (uint light = 0u; light < 3u; light++) {
            float d = -dot(in_normal, joints[base + 3u + light].xyz);
            if (d > 0.0) {
                shade += d * light_colour[light].rgb;
            }
        }
        shade = min(shade, vec3(255.0)) * tint.rgb / 255.0;
        shade *= in_colour;
    }
    out_uv = in_uv;
    out_shade = shade;
    out_packed = in_packed;
}
