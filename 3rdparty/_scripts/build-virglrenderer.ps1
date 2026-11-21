[CmdletBinding()]
param([switch] $Rebuild)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)

function Invoke-Native([scriptblock] $block) {
    $ErrorActionPreference = 'Continue'
    & $block 2>&1 | Out-Null
    return $LASTEXITCODE
}

function Invoke-Checked([string] $what, [scriptblock] $block) {
    $ErrorActionPreference = 'Continue'
    & $block
    if ($LASTEXITCODE -ne 0) { throw "$what failed ($LASTEXITCODE)" }
}

$build = Join-Path $root 'build'
$venv = Join-Path $build 'meson-venv'
$meson = Join-Path $venv 'Scripts\meson.exe'
$native = Join-Path $build 'native.ini'
$pcdir = Join-Path $build 'pkgconfig'

$virglrendererUpstream = Join-Path $root '3rdparty\virglrenderer'
$virglrendererPatches = Join-Path $root '3rdparty\_patches\virglrenderer'
$virglrendererSrc = Join-Path $build 'virglrenderer-src'
$virglrendererBuild = Join-Path $build 'virglrenderer-build'
$virglrendererPrefix = Join-Path $build 'virglrenderer'

if (-not (Test-Path (Join-Path $virglrendererUpstream 'meson.build'))) {
    Write-Error "$virglrendererUpstream is not checked out; run: git submodule update --init"
}

# What the installed library was built from: this script, the submodule's commit, the patches
# applied to it, where it is installed (its .pc names the prefix), and the Vulkan SDK if there
# is one. The same as the last build's, and nothing is built again -- which is also what lets
# CI keep it in a cache.
$virglrendererCommit = & git -C $virglrendererUpstream rev-parse HEAD
$patches = @(Get-ChildItem -Path $virglrendererPatches -Filter '*.patch' | Sort-Object Name)
$stamp = Join-Path $virglrendererPrefix '.virtx-stamp'
$want = (@(
    $build
    (Get-FileHash -Algorithm SHA256 $PSCommandPath).Hash
    $virglrendererCommit
    $patches | ForEach-Object { (Get-FileHash -Algorithm SHA256 $_.FullName).Hash }
    "$env:VULKAN_SDK"
) -join "`n").TrimEnd()
if (-not $Rebuild -and (Test-Path (Join-Path $virglrendererPrefix 'lib\pkgconfig\virglrenderer.pc')) -and
    (Test-Path $stamp) -and ((Get-Content -Raw $stamp).TrimEnd() -eq $want)) {
    Write-Output "virglrenderer up to date ($(& git -C $virglrendererUpstream rev-parse --short HEAD))"
    exit 0
}

New-Item -ItemType Directory -Force -Path $build, $pcdir | Out-Null

Write-Output 'Setting up the MSVC environment'

$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
if (-not (Test-Path $vswhere)) { Write-Error 'vswhere.exe not found; install the MSVC build tools' }
$vs = & $vswhere -latest -products * `
    -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath
if (-not $vs) { Write-Error 'no Visual Studio installation with the x64 C++ tools' }
cmd /c "`"$(Join-Path $vs 'VC\Auxiliary\Build\vcvars64.bat')`" >nul 2>&1 && set" | ForEach-Object {
    if ($_ -match '^([^=]+)=(.*)$') { Set-Item -Path "env:$($matches[1])" -Value $matches[2] }
}

# clang-cl, not cl: this is mesa-derived C, written against GCC builtins, function attributes
# and VLAs, and under cl configure answers NO to every `cc.has_function` and
# `cc.has_function_attribute`. clang-cl keeps the MSVC ABI, so the archive still links with the
# msvc Rust toolchain.
$clangcl = (Get-Command clang-cl.exe -ErrorAction SilentlyContinue).Source
if (-not $clangcl) { Write-Error 'clang-cl.exe is not on PATH; install LLVM' }
$lldLink = (Get-Command lld-link.exe -ErrorAction SilentlyContinue).Source
$llvmLib = (Get-Command llvm-lib.exe -ErrorAction SilentlyContinue).Source
if (-not (Get-Command pkg-config.exe -ErrorAction SilentlyContinue)) { Write-Error 'pkg-config.exe is not on PATH' }

if (-not (Test-Path $meson)) {
    $python = Get-Command python.exe, python3.exe -All -ErrorAction SilentlyContinue |
        Select-Object -ExpandProperty Source |
        Where-Object { (Invoke-Native { & $_ --version }) -eq 0 } |
        Select-Object -First 1
    if (-not $python) {
        throw 'no usable python.exe on PATH (the Microsoft Store stub does not count)'
    }
    Write-Output 'Creating the meson venv'
    Invoke-Checked 'venv' { & $python -m venv $venv }
    Invoke-Checked 'pip install' {
        & (Join-Path $venv 'Scripts\python.exe') -m pip install --quiet --upgrade pip meson pyyaml mako
    }
}
if ((Invoke-Native { & (Join-Path $venv 'Scripts\python.exe') -c 'import yaml, mako' }) -ne 0) {
    Invoke-Checked 'pip install' { & (Join-Path $venv 'Scripts\python.exe') -m pip install --quiet pyyaml mako }
}
# meson resolves `python3` off PATH -- a native-file `[binaries] python3` does not steer its
# python module -- and a Windows venv never creates that name.
$scripts = Join-Path $venv 'Scripts'
if (-not (Test-Path (Join-Path $scripts 'python3.exe'))) {
    Copy-Item (Join-Path $scripts 'python.exe') (Join-Path $scripts 'python3.exe')
}
$env:PATH = "$scripts;$env:PATH"

@"
[binaries]
c = '$($clangcl -replace '\\','/')'
$(if ($lldLink) { "c_ld = '$($lldLink -replace '\\','/')'" })
$(if ($llvmLib) { "ar = '$($llvmLib -replace '\\','/')'" })
"@ | Set-Content -Encoding ascii $native

# The Vulkan loader: headers, and an import library for `vulkan-1.dll`. Both come from the
# Vulkan SDK when one is installed (`VULKAN_SDK`), and otherwise from what is already here --
# the Vulkan headers virglrenderer carries in its tree, and an import library made from
# them. That one is enough because the host delay-loads `vulkan-1.dll`
# (virtx-uvm/build.rs): the link needs only the names, and a name the loader does
# not export would fail only if something called it. Any Windows
# with a GPU driver has the DLL itself; it is the driver's, not ours to ship.
#
# Searched for rather than assumed at `Include\` and `Lib\`, and the shallowest of each taken,
# since an SDK carries other copies (MoltenVK's, samples').
function Find-Shallowest([string] $dir, [string] $name) {
    if (-not $dir -or -not (Test-Path $dir)) { return $null }
    Get-ChildItem -Path $dir -Recurse -Filter $name -ErrorAction SilentlyContinue |
        Where-Object { $_.DirectoryName -notmatch '32$' } |
        Sort-Object { $_.FullName.Length } | Select-Object -First 1
}
$sdkVersion = if ($env:VULKAN_SDK_VERSION) { $env:VULKAN_SDK_VERSION } elseif ($env:VULKAN_SDK) { Split-Path -Leaf $env:VULKAN_SDK } else { 'bundled' }
$vulkanCore = Find-Shallowest $env:VULKAN_SDK 'vulkan_core.h'
if (-not $vulkanCore) {
    $vulkanCore = Find-Shallowest (Join-Path $virglrendererUpstream 'include') 'vulkan_core.h'
}
if (-not $vulkanCore) { Write-Error 'no Vulkan headers: neither VULKAN_SDK nor virglrenderer''s include\ has them' }
$vulkanInclude = Split-Path -Parent $vulkanCore.DirectoryName
$vulkanLib = Find-Shallowest $env:VULKAN_SDK 'vulkan-1.lib'
if ($vulkanLib) {
    $vulkanLibDir = $vulkanLib.DirectoryName
} else {
    $vulkanLibDir = Join-Path $build 'vulkan'
    New-Item -ItemType Directory -Force -Path $vulkanLibDir | Out-Null
    # vulkan_win32.h only where there is one: virglrenderer's tree has none, and venus
    # calls nothing in it by name.
    $headers = 'vulkan_core.h', 'vulkan_win32.h' |
        ForEach-Object { Join-Path $vulkanInclude "vulkan\$_" } | Where-Object { Test-Path $_ }
    $names = foreach ($header in $headers) {
        Select-String -Path $header `
            -Pattern 'VKAPI_ATTR\s+\S+\s+VKAPI_CALL\s+(vk\w+)\s*\(' |
            ForEach-Object { $_.Matches[0].Groups[1].Value }
    }
    $def = Join-Path $vulkanLibDir 'vulkan-1.def'
    (@('LIBRARY vulkan-1.dll', 'EXPORTS') + ($names | Sort-Object -Unique)) |
        Set-Content -Encoding ascii $def
    Invoke-Checked 'lib /def' {
        lib.exe /nologo /machine:x64 "/def:$def" "/out:$(Join-Path $vulkanLibDir 'vulkan-1.lib')"
    }
    Write-Output "  no Vulkan SDK import library; made vulkan-1.lib from the headers ($(@($names).Count) names)"
}
Write-Output "  vulkan: headers in $vulkanInclude, vulkan-1.lib in $vulkanLibDir"
@"
Name: vulkan
Description: Vulkan SDK loader
Version: $sdkVersion
Cflags: -I$($vulkanInclude -replace '\\','/')
Libs: -L$($vulkanLibDir -replace '\\','/') -lvulkan-1
"@ | Set-Content -Encoding ascii (Join-Path $pcdir 'vulkan.pc')

Write-Output 'Setting up virglrenderer'

$env:PKG_CONFIG_PATH = $pcdir

# The source built is the submodule's commit with the patches applied, in a tree of its own:
# the submodule stays as upstream has it.
Remove-Item -Recurse -Force $virglrendererSrc, $virglrendererBuild, $virglrendererPrefix -ErrorAction SilentlyContinue
Invoke-Checked 'virglrenderer clone' {
    & git -c advice.detachedHead=false clone -q -c core.autocrlf=false $virglrendererUpstream $virglrendererSrc
}
Invoke-Checked 'virglrenderer checkout' { & git -C $virglrendererSrc checkout -q --detach $virglrendererCommit }
foreach ($patch in $patches) {
    Invoke-Checked "virglrenderer patch $($patch.Name)" { & git -C $virglrendererSrc apply $patch.FullName }
}

# Static, so that nothing has to find a DLL at start-up -- and against the static C runtime
# (`b_vscrt=mt`), as the Rust side is (`.cargo/config.toml`): an archive built for the DLL one
# asks for `__imp_` symbols a static link has none of.
Write-Output "Configuring virglrenderer ($(& git -C $virglrendererUpstream rev-parse --short HEAD), patched)"
Invoke-Checked 'virglrenderer configure' {
    & $meson setup $virglrendererBuild $virglrendererSrc --prefix $virglrendererPrefix `
        --buildtype release --native-file $native `
        -Dvenus=true -Dvrend=false -Drender-server-mode=none -Dvulkan-dload=false -Dplatforms= `
        -Ddefault_library=static -Db_vscrt=mt | Out-Null
}

Write-Output 'Compiling virglrenderer'
# Kept in a log rather than printed, and a build that fails prints what failed of it -- in CI
# there is no tree left to look in afterwards.
$virglrendererLog = Join-Path $build 'virglrenderer-build.log'
Invoke-Checked 'virglrenderer build' {
    & $meson compile -C $virglrendererBuild *> $virglrendererLog
    if ($LASTEXITCODE -ne 0) {
        Select-String -Path $virglrendererLog -CaseSensitive -Context 3, 6 `
            -Pattern '^FAILED|error( [A-Z]+[0-9]+)?:' | Select-Object -First 20 | Out-String -Width 400
    }
}
Invoke-Checked 'virglrenderer install' { & $meson install -C $virglrendererBuild | Out-Null }

$virglrendererPcdir = Join-Path $virglrendererPrefix 'lib\pkgconfig'
Copy-Item (Join-Path $pcdir 'vulkan.pc') (Join-Path $virglrendererPcdir 'vulkan.pc') -Force

$env:PKG_CONFIG_PATH = $virglrendererPcdir
$version = & pkg-config --modversion virglrenderer
Write-Output "virglrenderer $version installed to $virglrendererPrefix"
Set-Content -Encoding ascii $stamp $want
