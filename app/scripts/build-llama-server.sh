#!/bin/sh
# Builds llama.cpp's llama-server, the process that runs the Echotype cleanup model (src-tauri/src/llm.rs),
# into src-tauri/binaries/ under the name Tauri's externalBin expects. macOS only. Run it once before
# building the app on a Mac; CI and the release workflow run it too.
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
  *) echo "build-llama-server.sh: macOS targets only, not $TRIPLE" >&2; exit 1 ;;
esac

[ -d "$DIR/src/.git" ] || git clone --quiet --depth 1 --branch "$TAG" https://github.com/ggml-org/llama.cpp "$DIR/src"
if [ "$(git -C "$DIR/src" rev-parse HEAD)" != "$COMMIT" ]; then
  echo "build-llama-server.sh: $DIR/src is not llama.cpp $TAG ($COMMIT); delete it and run again" >&2
  exit 1
fi

# Static, no OpenSSL, no web UI, Metal shaders embedded: one file that needs only system libraries.
cmake -S "$DIR/src" -B "$DIR/build-$ARCH" -DCMAKE_BUILD_TYPE=Release \
  -DCMAKE_OSX_ARCHITECTURES=$ARCH -DCMAKE_OSX_DEPLOYMENT_TARGET=13.0 \
  -DBUILD_SHARED_LIBS=OFF -DGGML_NATIVE=OFF -DGGML_CCACHE=OFF \
  -DGGML_METAL=$METAL -DGGML_METAL_EMBED_LIBRARY=$METAL \
  -DLLAMA_OPENSSL=OFF -DLLAMA_BUILD_TESTS=OFF -DLLAMA_BUILD_EXAMPLES=OFF \
  -DLLAMA_BUILD_UI=OFF -DLLAMA_USE_PREBUILT_UI=OFF >/dev/null
cmake --build "$DIR/build-$ARCH" --config Release -j "$(sysctl -n hw.ncpu)" --target llama-server

OUT="$APP/src-tauri/binaries/llama-server-$TRIPLE"
mkdir -p "$APP/src-tauri/binaries"
cp "$DIR/build-$ARCH/bin/llama-server" "$OUT"
if otool -L "$OUT" | tail -n +2 | grep -v -e '^[[:space:]]*/usr/lib/' -e '^[[:space:]]*/System/Library/'; then
  echo "build-llama-server.sh: $OUT links a library outside the system (above)" >&2
  exit 1
fi
echo "built $OUT (llama.cpp $TAG)"
