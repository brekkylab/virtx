# Build and run guest/vk_smoke.c inside the VM, on a Windows host.
#
# The POSIX script next to this one does the same thing through `macos_gpu`.
# The rootfs is a plain host directory shared over virtio-fs, so the source is
# just copied in and compiled by the guest's own toolchain -- no cross
# compilation. The build deps are installed on first run and persist in the
# rootfs.
#
#   .\scripts\run-vk-smoke.ps1
#
[CmdletBinding()]
param()

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$rootfs = Join-Path $root 'data\rootfs\alpine'

if (-not (Test-Path $rootfs)) {
    Write-Error "no rootfs at $rootfs; run .\scripts\fetch-alpine-rootfs.ps1"
}

New-Item -ItemType Directory -Force -Path (Join-Path $rootfs 'root') | Out-Null
Copy-Item (Join-Path $root 'guest\vk_smoke.c') (Join-Path $rootfs 'root\') -Force

# No double quotes in the guest script: the argument goes through PowerShell and
# CreateProcess on its way to `/bin/sh -c`, and a quoted run of words does not
# survive the trip intact.
$guest = @'
set -e
command -v cc >/dev/null 2>&1 || apk add --no-cache -q build-base
[ -e /usr/include/vulkan/vulkan.h ] || apk add --no-cache -q vulkan-headers vulkan-loader-dev
[ -e /usr/share/vulkan/icd.d/virtio_icd.x86_64.json ] || apk add --no-cache -q mesa-vulkan-virtio vulkan-loader
cd /root
cc -O2 -Wall -o vk_smoke vk_smoke.c -lvulkan
./vk_smoke
'@

# Windows PowerShell turns anything a native program writes to stderr into an
# error record, and with ErrorActionPreference=Stop that ends the script -- so
# cargo's build warnings would look like a failure. The exit code is the thing
# to trust here.
$ErrorActionPreference = 'Continue'
& cargo run --features gpu --example win_gpu -- $guest
exit $LASTEXITCODE
