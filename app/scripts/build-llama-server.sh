#!/bin/sh
# Builds llama.cpp's llama-server, the process that runs the Echotype cleanup model (src-tauri/src/llm.rs),
# into src-tauri/binaries/ under the name Tauri's externalBin expects. macOS and Windows (Git Bash,
# Visual Studio and the Vulkan SDK, VULKAN_SDK set). Run it once before building the app; CI and the
# release workflow run it too. On Windows it also copies the Vulkan loader (vulkan-1.dll) the app ships.
#   app/scripts/build-llama-server.sh [target triple]      default: this machine's
#   LLAMA_DIR=<dir>   where the llama.cpp source and build go (default: src-tauri/target/llama.cpp)
set -eu
TAG=b11619
COMMIT=9cf967a04d0dd39f648764d73ac07facc7b75db1 # llama.cpp is MIT: https://github.com/ggml-org/llama.cpp
APP=$(cd "$(dirname "$0")/.." && pwd)
TRIPLE=${1:-$(rustc -vV | sed -n 's/^host: //p')}
DIR=${LLAMA_DIR:-$APP/src-tauri/target/llama.cpp}
case $TRIPLE in
  aarch64-apple-darwin) ARCH=arm64 METAL=ON ;;
  # ponytail: CPU only on Intel Macs (ggml's Metal backend targets Apple GPUs); fine for a 1.5B model.
  x86_64-apple-darwin) ARCH=x86_64 METAL=OFF ;;
  # Vulkan: one build for NVIDIA, AMD and Intel GPUs; with no Vulkan GPU it runs on the CPU.
  x86_64-pc-windows-msvc) ARCH=x64 METAL=OFF ;;
  *) echo "build-llama-server.sh: macOS and Windows x64 targets only, not $TRIPLE" >&2; exit 1 ;;
esac
case $TRIPLE in
  *windows*) EXTRA="-DGGML_VULKAN=ON -DCMAKE_MSVC_RUNTIME_LIBRARY=MultiThreaded" EXE=.exe ;;
  *) EXTRA="-DCMAKE_OSX_ARCHITECTURES=$ARCH -DCMAKE_OSX_DEPLOYMENT_TARGET=13.0" EXE= ;;
esac

# Missing, half-restored from a CI cache, or another version: start from a clean clone of the pin.
if [ "$(git -C "$DIR/src" rev-parse HEAD 2>/dev/null)" != "$COMMIT" ]; then
  rm -rf "$DIR/src"
  git clone --quiet --depth 1 --branch "$TAG" https://github.com/ggml-org/llama.cpp "$DIR/src"
  if [ "$(git -C "$DIR/src" rev-parse HEAD)" != "$COMMIT" ]; then
    echo "build-llama-server.sh: llama.cpp $TAG is no longer $COMMIT upstream; check the tag" >&2
    exit 1
  fi
fi

# Static, no OpenSSL, no web UI, Metal shaders embedded: one file that needs only system libraries
# (on Windows, the static C runtime and vulkan-1.dll).
# ponytail: GGML_NATIVE=OFF builds the CPU code for AVX2 (2013+ x64 CPUs); older ones need
# GGML_CPU_ALL_VARIANTS with dynamic backends.
# shellcheck disable=SC2086
cmake -S "$DIR/src" -B "$DIR/build-$ARCH" -DCMAKE_BUILD_TYPE=Release $EXTRA \
  -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF -DGGML_CCACHE=OFF \
  -DGGML_METAL=$METAL -DGGML_METAL_EMBED_LIBRARY=$METAL \
  -DLLAMA_OPENSSL=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF \
  -DLLAMA_BUILD_UI=OFF -DLLAMA_USE_PREBUILT_UI=OFF >/dev/null
cmake --build "$DIR/build-$ARCH" --config Release -j "$(getconf _NPROCESSORS_ONLN)" --target llama-server

OUT="$APP/src-tauri/binaries/llama-server-$TRIPLE$EXE"
mkdir -p "$APP/src-tauri/binaries"
if [ -n "$EXE" ]; then
  cp "$DIR/build-$ARCH/bin/Release/llama-server.exe" "$OUT"
  # The Vulkan loader, shipped next to Echotype.exe so a PC without a GPU driver still starts (on the CPU).
  cp "$(cygpath -u "$VULKAN_SDK")/runtime/x64/vulkan-1.dll" "$APP/src-tauri/binaries/vulkan-1.dll" 2>/dev/null ||
    cp /c/Windows/System32/vulkan-1.dll "$APP/src-tauri/binaries/vulkan-1.dll"
  echo "built $OUT (llama.cpp $TAG, Vulkan)"
  exit 0
fi
cp "$DIR/build-$ARCH/bin/llama-server" "$OUT"
if otool -L "$OUT" | tail -n +2 | grep -v -e '^[[:space:]]*/usr/lib/' -e '^[[:space:]]*/System/Library/'; then
  echo "build-llama-server.sh: $OUT links a library outside the system (above)" >&2
  exit 1
fi
echo "built $OUT (llama.cpp $TAG)"
