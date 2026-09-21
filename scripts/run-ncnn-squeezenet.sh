#!/bin/sh
# Build ncnn with its Vulkan backend inside the VM and classify an image with
# SqueezeNet v1.1 on the guest GPU.
#
# Unlike run-vk-smoke.sh this does not copy sources into the rootfs: the ncnn
# checkout, its build tree and the model are build artefacts, so they live in
# data/inference/, which macos_inference shares with the guest over a second
# virtiofs device. The ncnn build only happens on the first run.
#
#   ./scripts/run-ncnn-squeezenet.sh [image]
set -eu

# ncnn is tagged by date. Bump deliberately; the model files are read from the
# same tag so the pair always matches.
NCNN_TAG="${NCNN_TAG:-20260526}"
NCNN_RAW="https://raw.githubusercontent.com/Tencent/ncnn/$NCNN_TAG/examples"
# Samoyed. The classic torchvision sample, and an unambiguous ImageNet class.
DEFAULT_IMAGE_URL="https://raw.githubusercontent.com/pytorch/hub/master/images/dog.jpg"

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
WORK="$ROOT/data/inference"
ASSETS="$WORK/assets"

[ -d "$ROOT/data/rootfs/alpine" ] || {
    echo "no rootfs; run ./scripts/fetch-alpine-rootfs.sh" >&2; exit 1
}

mkdir -p "$ASSETS" "$WORK/src"

fetch() {
    [ -e "$2" ] && return 0
    echo "fetching $(basename "$2")"
    curl -fsSL -o "$2" "$1"
}

fetch "$NCNN_RAW/squeezenet_v1.1.param" "$ASSETS/squeezenet_v1.1.param"
fetch "$NCNN_RAW/squeezenet_v1.1.bin" "$ASSETS/squeezenet_v1.1.bin"
fetch "$NCNN_RAW/synset_words.txt" "$ASSETS/synset_words.txt"

if [ $# -ge 1 ]; then
    IMAGE="$(basename "$1")"
    cp "$1" "$ASSETS/$IMAGE"
else
    IMAGE="dog.jpg"
    fetch "$DEFAULT_IMAGE_URL" "$ASSETS/$IMAGE"
fi

# Cloned on the host even though the guest has network through TSI: the host's
# is faster, and this keeps the checkout as a cached artefact under data/.
# No submodules -- NCNN_SYSTEM_GLSLANG below uses Alpine's glslang instead of
# the bundled one, which is the only submodule the Vulkan build needs.
if [ ! -d "$WORK/ncnn/.git" ]; then
    echo "cloning ncnn $NCNN_TAG"
    git clone --quiet --depth 1 --branch "$NCNN_TAG" \
        https://github.com/Tencent/ncnn "$WORK/ncnn"
fi

cp "$ROOT/guest/ncnn_squeezenet.cpp" "$WORK/src/"

exec cargo run --features gpu --example macos_inference -- "
    set -e
    W=/mnt/inference

    command -v cmake >/dev/null 2>&1 || apk add --no-cache -q build-base cmake samurai
    # spirv-tools-dev is not optional: glslang-config.cmake does a
    # find_dependency(SPIRV-Tools-opt), and without it find_package(glslang)
    # fails quietly, NCNN_SYSTEM_GLSLANG switches itself off, and the build
    # dies asking for the glslang submodule a shallow clone does not have.
    [ -e /usr/lib/cmake/SPIRV-Tools-opt/SPIRV-Tools-optConfig.cmake ] || apk add --no-cache -q glslang-dev spirv-tools-dev
    [ -e /usr/share/vulkan/icd.d/virtio_icd.aarch64.json ] || apk add --no-cache -q mesa-vulkan-virtio vulkan-loader

    if [ ! -e \"\$W/prefix/lib/libncnn.so\" ]; then
        echo 'building ncnn (first run only, this takes a while)'
        # CMAKE_POLICY_VERSION_MINIMUM: ncnn still declares a 2.8.12 floor and
        # cmake 4 refuses anything below 3.5.
        cmake -S \"\$W/ncnn\" -B \"\$W/build-ncnn\" -G Ninja \
            -DCMAKE_BUILD_TYPE=Release \
            -DCMAKE_INSTALL_PREFIX=\"\$W/prefix\" \
            -DCMAKE_POLICY_VERSION_MINIMUM=3.5 \
            -DNCNN_VULKAN=ON \
            -DNCNN_SYSTEM_GLSLANG=ON \
            -DNCNN_SHARED_LIB=ON \
            -DNCNN_SIMPLEOCV=ON \
            -DNCNN_BUILD_TOOLS=OFF \
            -DNCNN_BUILD_EXAMPLES=OFF \
            -DNCNN_BUILD_BENCHMARK=OFF >/dev/null
        cmake --build \"\$W/build-ncnn\" --parallel \"\$(nproc)\"
        cmake --install \"\$W/build-ncnn\" >/dev/null
    fi

    mkdir -p \"\$W/build\"
    g++ -O2 -o \"\$W/build/ncnn_squeezenet\" \"\$W/src/ncnn_squeezenet.cpp\" \
        -I\"\$W/prefix/include/ncnn\" -L\"\$W/prefix/lib\" -lncnn \
        -Wl,-rpath,\"\$W/prefix/lib\"

    # 2>&1 in the guest: stdout and stderr are separate virtio-console
    # ports, drained independently on the host, so ncnn's probe logs
    # interleave mid-line with our own output unless they share one fd.
    \"\$W/build/ncnn_squeezenet\" \"\$W/assets\" \"\$W/assets/$IMAGE\" 2>&1
"
