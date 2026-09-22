# Build virglrenderer with venus (Vulkan) for a Windows host.
#
# The POSIX script next to this one cannot do the job here. Three things
# differ, and each one decides something about the result:
#
#   the compiler   clang-cl, not cl. This is mesa-derived C, written against
#                  GCC builtins, function attributes and VLAs -- configure
#                  answers NO to every one of them under cl. clang-cl keeps the
#                  MSVC ABI, so the DLL still links with the msvc Rust
#                  toolchain.
#
#   libepoxy       virglrenderer requires it unconditionally: `virgl_sources +=
#                  vrend_sources` has no guard, and epoxy is in that target's
#                  dependency list. There is no venus-only build to opt into,
#                  so the submodule is built here first. Windows has no EGL or
#                  GLX, so only the WGL half is built and no GL winsys backend
#                  ends up in virglrenderer.
#
#   two patches    patches/virglrenderer-1.3.0-macos-venus.patch carries the
#                  in-process venus context, which is needed on any host
#                  without the out-of-process render server (fork, epoll,
#                  seccomp) -- Windows included, despite the name. The Windows
#                  patch is cut against a tree that already has it, so the
#                  order below is not optional.
#
# Everything lands under data/, which is untracked; the submodules stay
# pristine, the same way scripts/build-virglrenderer.sh keeps them.
#
#   .\scripts\build-virglrenderer-windows.ps1
#   .\scripts\build-virglrenderer-windows.ps1 -Fresh   # discard data/build too
#
[CmdletBinding()]
param([switch] $Fresh)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$data = Join-Path $root 'data'
$venv = Join-Path $data 'meson-venv'
$meson = Join-Path $venv 'Scripts\meson.exe'
$build = Join-Path $data 'build'
$src = Join-Path $data 'virglrenderer-src'
$prefix = Join-Path $data 'virglrenderer'
$epoxySrc = Join-Path $root '3rdparty\libepoxy'
$epoxyPrefix = Join-Path $data 'epoxy'

foreach ($sub in @($epoxySrc, (Join-Path $root '3rdparty\virglrenderer'))) {
    if (-not (Test-Path (Join-Path $sub 'meson.build'))) {
        Write-Error "$sub is not checked out; run: git submodule update --init"
    }
}

if ($Fresh) { Remove-Item -Recurse -Force $build, $src, $prefix, $epoxyPrefix -ErrorAction SilentlyContinue }
New-Item -ItemType Directory -Force -Path $build | Out-Null

# --- the MSVC environment -----------------------------------------------
# clang-cl finds the Windows SDK and the MSVC headers through the environment
# vcvars64 sets, so this has to happen before meson probes the compiler.
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { Write-Error 'vswhere.exe not found; install the MSVC build tools' }
$vs = & $vswhere -latest -products * `
    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { Write-Error 'no Visual Studio installation with the x64 C++ tools' }
cmd /c "`"$(Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat')`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path "env:$($matches[1])" -Value $matches[2] }
}

$clangcl = (Get-Command clang-cl.exe -ErrorAction SilentlyContinue).Source
if (-not $clangcl) { Write-Error 'clang-cl.exe is not on PATH; install LLVM' }
$lldLink = (Get-Command lld-link.exe -ErrorAction SilentlyContinue).Source
$llvmLib = (Get-Command llvm-lib.exe -ErrorAction SilentlyContinue).Source
if (-not $env:VULKAN_SDK) { Write-Error 'VULKAN_SDK is not set; install the Vulkan SDK' }

# --- meson, in a venv of its own ----------------------------------------
# Not installed into whatever Python happens to be on PATH: this needs pyyaml
# for gallium's codegen, and that is this build's business, not the system's.
if (-not (Test-Path $meson)) {
    $python = (Get-Command python.exe -ErrorAction SilentlyContinue).Source
    if (-not $python -or -not (& $python --version 2>$null)) {
        Write-Error 'no usable python.exe on PATH (the Microsoft Store stub does not count)'
    }
    Write-Output '==> creating the meson venv'
    & $python -m venv $venv
    & (Join-Path $venv 'Scripts\python.exe') -m pip install --quiet --upgrade pip meson pyyaml
}
# gallium's codegen is found as `python3`, which meson resolves off PATH -- a
# native-file [binaries] entry does not steer the python module -- and a
# Windows venv does not create that name.
$scripts = Join-Path $venv 'Scripts'
if (-not (Test-Path (Join-Path $scripts 'python3.exe'))) {
    Copy-Item (Join-Path $scripts 'python.exe') (Join-Path $scripts 'python3.exe')
}
$env:PATH = "$scripts;$env:PATH"

$native = Join-Path $build 'native.ini'
@"
[binaries]
c = '$($clangcl -replace '\\','/')'
$(if ($lldLink) { "c_ld = '$($lldLink -replace '\\','/')'" })
$(if ($llvmLib) { "ar = '$($llvmLib -replace '\\','/')'" })
"@ | Set-Content -Encoding ascii $native

# --- libepoxy -----------------------------------------------------------
if (-not (Test-Path (Join-Path $epoxyPrefix 'lib\pkgconfig\epoxy.pc'))) {
    Write-Output '==> building libepoxy'
    $epoxyBuild = Join-Path $build 'libepoxy'
    Remove-Item -Recurse -Force $epoxyBuild -ErrorAction SilentlyContinue
    & $meson setup $epoxyBuild $epoxySrc --prefix $epoxyPrefix --buildtype release `
        --native-file $native -Dtests=false -Ddocs=false | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Error 'libepoxy configure failed' }
    & $meson compile -C $epoxyBuild | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Error 'libepoxy build failed' }
    & $meson install -C $epoxyBuild | Out-Null
}

# --- the patched virglrenderer source -----------------------------------
if (-not (Test-Path (Join-Path $src '.git'))) {
    Write-Output '==> exporting the pinned submodule'
    Remove-Item -Recurse -Force $src -ErrorAction SilentlyContinue
    New-Item -ItemType Directory -Force -Path $src | Out-Null
    # robocopy, not `git archive | tar`: Windows bsdtar refuses some of the
    # tree's dotfiles. /XF .git matters -- a submodule's .git is a *file*, and
    # copying it points every later git command at the submodule's own repo.
    # Exit codes below 8 are success.
    & robocopy (Join-Path $root '3rdparty\virglrenderer') $src /E /XD .git /XF .git /NFL /NDL /NJH /NJS /NP | Out-Null
    if ($LASTEXITCODE -ge 8) { Write-Error "copying the submodule failed ($LASTEXITCODE)" }
    $global:LASTEXITCODE = 0

    & git -C $src init -q
    foreach ($patch in @('virglrenderer-1.3.0-macos-venus.patch',
                         'virglrenderer-1.3.0-windows.patch')) {
        Write-Output "==> applying $patch"
        & git -C $src apply (Join-Path $root "patches\$patch")
        if ($LASTEXITCODE -ne 0) { Write-Error "$patch does not apply" }
    }
}

# --- pkg-config ---------------------------------------------------------
# The Vulkan SDK ships no .pc file, and meson's dependency('vulkan') wants one.
# No `prefix` variable in it: pkgconf rewrites that to the .pc file's own
# grandparent, which silently points the include path at data/.
$pcdir = Join-Path $build 'pkgconfig'
New-Item -ItemType Directory -Force -Path $pcdir | Out-Null
$sdk = $env:VULKAN_SDK -replace '\\','/'
$sdkVersion = Split-Path -Leaf $env:VULKAN_SDK
@"
Name: vulkan
Description: Vulkan SDK loader
Version: $sdkVersion
Cflags: -I$sdk/Include
Libs: -L$sdk/Lib -lvulkan-1
"@ | Set-Content -Encoding ascii (Join-Path $pcdir 'vulkan.pc')

$env:PKG_CONFIG_PATH = "$pcdir;$(Join-Path $epoxyPrefix 'lib\pkgconfig')"

# --- virglrenderer ------------------------------------------------------
$virglBuild = Join-Path $build 'virglrenderer'
if (-not (Test-Path (Join-Path $virglBuild 'build.ninja'))) {
    Write-Output '==> configuring virglrenderer'
    # -Dplatforms= keeps EGL and GLX out: neither exists here, and venus wants
    # neither. -Drender-server=false is what the venus patch restores the
    # option for. -Dvulkan-dload=false because the in-process venus context
    # calls vkMapMemory directly and so needs Vulkan linked.
    & $meson setup $virglBuild $src --prefix $prefix --buildtype release --native-file $native `
        -Dvenus=true -Drender-server=false -Dvulkan-dload=false -Dplatforms= | Out-Null
    if ($LASTEXITCODE -ne 0) { Write-Error 'virglrenderer configure failed' }
}

Write-Output '==> building virglrenderer'
& $meson compile -C $virglBuild | Out-Null
if ($LASTEXITCODE -ne 0) { Write-Error 'virglrenderer build failed' }
& $meson install -C $virglBuild | Out-Null

# Keep everything consumers need behind a single PKG_CONFIG_PATH entry, the
# same bargain scripts/build-virglrenderer.sh strikes on macOS.
Copy-Item (Join-Path $pcdir 'vulkan.pc') (Join-Path $prefix 'lib\pkgconfig\vulkan.pc') -Force
Copy-Item (Join-Path $epoxyPrefix 'lib\pkgconfig\epoxy.pc') (Join-Path $prefix 'lib\pkgconfig\epoxy.pc') -Force

Write-Output ''
Write-Output "installed to $prefix"
$env:PKG_CONFIG_PATH = Join-Path $prefix 'lib\pkgconfig'
& pkg-config --modversion virglrenderer | ForEach-Object { "  virglrenderer $_" }
Write-Output ''
Write-Output 'virglrenderer-1.dll needs epoxy-0.dll beside it or on PATH.'
