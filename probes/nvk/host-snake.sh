#!/bin/sh
# Builds vk_snake.c for the host (-DSNAKE_HOST: the system's Vulkan, offscreen, a fixed 60 Hz step) and runs it.
#   probes/nvk/host-snake.sh <dump-dir> <frame,frame,...> [frames]   -> <dump-dir>/frameNNNNN.ppm
# Other variables: SNAKE3D_SIZE=1920x1080 SNAKE3D_CAMERA=1 SNAKE3D_START_AT=<frame> SNAKE3D_DIE_AT=<frame> SNAKE3D_SEED=<n>
set -e
here=$(cd "$(dirname "$0")" && pwd)
out=${1:?dump dir}; at=${2:?frames}; n=${3:-600}
mkdir -p "$out"
${CC:-clang} -O1 -g -Wall -Wextra -Wno-unused-parameter -DSNAKE_HOST -I"$here" "$here/vk_snake.c" -o "$out/snake3d-host" -lvulkan -lm
SNAKE3D_DUMP="$out" SNAKE3D_DUMP_AT="$at" SNAKE3D_FRAMES="$n" "$out/snake3d-host"
