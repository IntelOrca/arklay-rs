#!/usr/bin/env bash
# Recompile the GPU renderer's GLSL to the SPIR-V the engine embeds. Needs
# `glslc` (shaderc). The compiled `.spv` files are committed so a build never
# needs the shader toolchain; rerun this after editing a shader.
set -euo pipefail
cd "$(dirname "$0")"
for source in *.vert *.frag; do
    glslc -O --target-env=vulkan1.0 -o "$source.spv" "$source"
done
