# Build ncnn with its Vulkan backend inside the VM and classify an image with
# SqueezeNet v1.1 on the guest GPU, on a Windows host.
#
# The POSIX script next to this one does the same thing through
# `macos_inference`. As there, nothing is copied into the rootfs: the ncnn
# checkout, its build tree and the model are build artefacts, so they live in
# data/inference/, which `win_inference` shares with the guest over a second
# virtiofs device. The ncnn build only happens on the first run.
#
#   .\scripts\run-ncnn-squeezenet.ps1 [image]
#
[CmdletBinding()]
param([string] $Image)

$ErrorActionPreference = 'Stop'

# ncnn is tagged by date. Bump deliberately; the model files are read from the
# same tag so the pair always matches.
$ncnnTag = if ($env:NCNN_TAG) { $env:NCNN_TAG } else { '20260526' }
$ncnnRaw = "https://raw.githubusercontent.com/Tencent/ncnn/$ncnnTag/examples"
# Samoyed. The classic torchvision sample, and an unambiguous ImageNet class.
$defaultImageUrl = 'https://raw.githubusercontent.com/pytorch/hub/master/images/dog.jpg'

$root = Split-Path -Parent $PSScriptRoot
$work = Join-Path $root 'data\inference'
$assets = Join-Path $work 'assets'

if (-not (Test-Path (Join-Path $root 'data\rootfs\alpine'))) {
    Write-Error 'no rootfs; run .\scripts\fetch-alpine-rootfs.ps1'
}

New-Item -ItemType Directory -Force -Path $assets, (Join-Path $work 'src') | Out-Null

function Fetch($url, $dest) {
    if (Test-Path $dest) { return }
    Write-Output "fetching $(Split-Path -Leaf $dest)"
    Invoke-WebRequest -UseBasicParsing -Uri $url -OutFile $dest
}

Fetch "$ncnnRaw/squeezenet_v1.1.param" (Join-Path $assets 'squeezenet_v1.1.param')
Fetch "$ncnnRaw/squeezenet_v1.1.bin"   (Join-Path $assets 'squeezenet_v1.1.bin')
Fetch "$ncnnRaw/synset_words.txt"      (Join-Path $assets 'synset_words.txt')

if ($Image) {
    $imageName = Split-Path -Leaf $Image
    Copy-Item $Image (Join-Path $assets $imageName) -Force
} else {
    $imageName = 'dog.jpg'
    Fetch $defaultImageUrl (Join-Path $assets $imageName)
}

# Cloned on the host even though the guest has network through TSI: the host's
# is faster, and this keeps the checkout as a cached artefact under data/.
# No submodules -- NCNN_SYSTEM_GLSLANG below uses Alpine's glslang instead of
# the bundled one, which is the only submodule the Vulkan build needs.
if (-not (Test-Path (Join-Path $work 'ncnn\.git'))) {
    Write-Output "cloning ncnn $ncnnTag"
    & git clone --quiet --depth 1 --branch $ncnnTag `
        https://github.com/Tencent/ncnn (Join-Path $work 'ncnn')
    if ($LASTEXITCODE -ne 0) { Write-Error 'ncnn clone failed' }
}

Copy-Item (Join-Path $root 'guest\ncnn_squeezenet.cpp') (Join-Path $work 'src\') -Force

# The script is handed to the guest as a file rather than as an argument.
# `cargo run -- <script>` reaches the binary through `powershell -File
# scripts/windows-run.ps1`, and PowerShell's -File argument parsing does not
# carry a multi-line string intact: indented lines vanish outright, quoted runs
# of words lose everything after the first space, and `!` arrives as `'\!'` --
# all silently, so the guest runs a script with holes in it and fails somewhere
# unrelated. data/inference is already shared with the guest, so the script can
# simply be written there and the one short argument names it.
$guest = @'
set -e
W=/mnt/inference

command -v cmake >/dev/null 2>&1 || apk add --no-cache -q build-base cmake samurai
# spirv-tools-dev is not optional: glslang-config.cmake does a
# find_dependency(SPIRV-Tools-opt), and without it find_package(glslang)
# fails quietly, NCNN_SYSTEM_GLSLANG switches itself off, and the build
# dies asking for the glslang submodule a shallow clone does not have.
[ -e /usr/lib/cmake/SPIRV-Tools-opt/SPIRV-Tools-optConfig.cmake ] || apk add --no-cache -q glslang-dev spirv-tools-dev
[ -e /usr/share/vulkan/icd.d/virtio_icd.x86_64.json ] || apk add --no-cache -q mesa-vulkan-virtio vulkan-loader

# CMAKE_POLICY_VERSION_MINIMUM: ncnn still declares a 2.8.12 floor and cmake 4
# refuses anything below 3.5.
build_ncnn() {
  echo building ncnn -- first run only, this takes a while
  cmake -S $W/ncnn -B $W/build-ncnn -G Ninja -DCMAKE_BUILD_TYPE=Release -DCMAKE_INSTALL_PREFIX=$W/prefix -DCMAKE_POLICY_VERSION_MINIMUM=3.5 -DNCNN_VULKAN=ON -DNCNN_SYSTEM_GLSLANG=ON -DNCNN_SHARED_LIB=ON -DNCNN_SIMPLEOCV=ON -DNCNN_BUILD_TOOLS=OFF -DNCNN_BUILD_EXAMPLES=OFF -DNCNN_BUILD_BENCHMARK=OFF >/dev/null
  cmake --build $W/build-ncnn --parallel $(nproc)
  cmake --install $W/build-ncnn >/dev/null
}
[ -e $W/prefix/lib/libncnn.so ] || build_ncnn

mkdir -p $W/build
g++ -O2 -o $W/build/ncnn_squeezenet $W/src/ncnn_squeezenet.cpp -I$W/prefix/include/ncnn -L$W/prefix/lib -lncnn -Wl,-rpath,$W/prefix/lib

# 2>&1 in the guest: stdout and stderr are separate virtio-console ports,
# drained independently on the host, so ncnn's probe logs interleave mid-line
# with our own output unless they share one fd.
$W/build/ncnn_squeezenet $W/assets $W/assets/@IMAGE@ 2>&1
'@

# LF, not CRLF: `sh` takes the carriage return as part of each command and
# every line fails. Set-Content would write CRLF, hence WriteAllText.
[IO.File]::WriteAllText(
    (Join-Path $work 'run.sh'),
    $guest.Replace('@IMAGE@', $imageName).Replace("`r`n", "`n"))

# Windows PowerShell turns anything a native program writes to stderr into an
# error record, and with ErrorActionPreference=Stop that ends the script -- so
# cargo's build warnings would look like a failure. The exit code is the thing
# to trust here.
$ErrorActionPreference = 'Continue'
& cargo run --features gpu --example win_inference -- 'sh /mnt/inference/run.sh'
exit $LASTEXITCODE
