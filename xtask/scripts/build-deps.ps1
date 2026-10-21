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

Write-Output 'Setting up the MSVC environment'

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
if (-not (Get-Command pkg-config.exe -ErrorAction SilentlyContinue)) { Write-Error 'pkg-config.exe is not on PATH' }

$build = Join-Path $root 'build'
$venv = Join-Path $build 'meson-venv'
$meson = Join-Path $venv 'Scripts\meson.exe'
$native = Join-Path $build 'native.ini'
$pcdir = Join-Path $build 'pkgconfig'

$epoxySrc = Join-Path $root '3rdparty\libepoxy'
$epoxyBuild = Join-Path $build 'epoxy-build'
$epoxyPrefix = Join-Path $build 'epoxy'

$virglrendererSrc = Join-Path $root '3rdparty\virglrenderer'
$virglrendererBuild = Join-Path $build 'virglrenderer-build'
$virglrendererPrefix = Join-Path $build 'virglrenderer'

foreach ($sub in @($epoxySrc, $virglrendererSrc)) {
    if (-not (Test-Path (Join-Path $sub 'meson.build'))) {
        Write-Error "$sub is not checked out; run: git submodule update --init"
    }
}

if ($Rebuild) {
    Remove-Item -Recurse -Force $epoxyBuild, $epoxyPrefix, $virglrendererBuild, $virglrendererPrefix `
        -ErrorAction SilentlyContinue
}
New-Item -ItemType Directory -Force -Path $build, $pcdir | Out-Null

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
    $vulkanCore = Find-Shallowest (Join-Path $virglrendererSrc 'include') 'vulkan_core.h'
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

Write-Output 'Setting up libepoxy'

# Static, like virglrenderer, so that nothing has to find a DLL at start-up -- and against the
# static C runtime (`b_vscrt=mt`), as the Rust side is (`.cargo/config.toml`): an archive
# built for the DLL one asks for `__imp_` symbols a static link has none of.
#
# `EPOXY_PUBLIC` is defined on the command line and not left to the headers, because
# `include/epoxy/common.h` defaults it to `__declspec(dllimport)` under any compiler that
# defines `_MSC_VER` -- clang-cl does -- and libepoxy's meson sets it only for a shared
# build. It has to be defined for libepoxy's own compile and not just for its consumers':
# without it `dispatch_common.c` asks for `__imp_epoxy_wglGetProcAddress`, a symbol its own
# archive is what defines.
if (Test-Path (Join-Path $epoxyBuild 'build.ninja')) { $reconfigure = @('--reconfigure') } else { $reconfigure = @() }
Invoke-Checked 'libepoxy configure' {
    & $meson setup @reconfigure $epoxyBuild $epoxySrc --prefix $epoxyPrefix --buildtype release `
        --native-file $native -Dtests=false -Ddocs=false -Ddefault_library=static -Db_vscrt=mt `
        -Dc_args=-DEPOXY_PUBLIC=extern | Out-Null
}
Invoke-Checked 'libepoxy build' { & $meson compile -C $epoxyBuild | Out-Null }
Invoke-Checked 'libepoxy install' { & $meson install -C $epoxyBuild | Out-Null }

# epoxy's `.pc`, written rather than read, for the two things an archive needs and the
# installed one does not say: the same `EPOXY_PUBLIC`, for everything that includes its
# headers, and `-lopengl32 -lgdi32` in `Libs` rather than `Libs.private` -- an archive does
# not carry its own dependencies, and the `pkg_config` crate reads the private ones only
# for a probe asked to be static, which `rutabaga_gfx`'s build script does not do. It goes
# in $pcdir, which is the whole of the path below, so this is the one both virglrenderer's
# build and that probe resolve.
$epoxy = $epoxyPrefix -replace '\\','/'
$epoxyVersion = (Select-String -Path (Join-Path $epoxyPrefix 'lib\pkgconfig\epoxy.pc') `
    -Pattern '^Version:\s*(.+)$').Matches[0].Groups[1].Value.Trim()
@"
Name: epoxy
Description: GL dispatch library
Version: $epoxyVersion
Libs: -L$epoxy/lib -lepoxy -lopengl32 -lgdi32
Cflags: -I$epoxy/include -DEPOXY_PUBLIC=extern
"@ | Set-Content -Encoding ascii (Join-Path $pcdir 'epoxy.pc')

Write-Output 'Setting up virglrenderer'

$env:PKG_CONFIG_PATH = $pcdir

if (Test-Path (Join-Path $virglrendererBuild 'build.ninja')) { $reconfigure = @('--reconfigure') } else { $reconfigure = @() }
Write-Output "Configuring virglrenderer ($(& git -C $virglrendererSrc rev-parse --short HEAD))"
Invoke-Checked 'virglrenderer configure' {
    & $meson setup @reconfigure $virglrendererBuild $virglrendererSrc --prefix $virglrendererPrefix `
        --buildtype release --native-file $native `
        -Dvenus=true -Drender-server-mode=none -Dvulkan-dload=false -Dplatforms= `
        -Ddefault_library=static -Db_vscrt=mt | Out-Null
}

Write-Output 'Compiling virglrenderer'
# Kept in a log rather than dropped, so that a build that fails can say why -- in CI there is
# no tree left to look in afterwards.
$virglrendererLog = Join-Path $build 'virglrenderer-build.log'
Invoke-Checked 'virglrenderer build' { & $meson compile -C $virglrendererBuild *> $virglrendererLog }
Invoke-Checked 'virglrenderer install' { & $meson install -C $virglrendererBuild | Out-Null }

$virglrendererPcdir = Join-Path $virglrendererPrefix 'lib\pkgconfig'
foreach ($pc in 'vulkan.pc', 'epoxy.pc') {
    Copy-Item (Join-Path $pcdir $pc) (Join-Path $virglrendererPcdir $pc) -Force
}

$env:PKG_CONFIG_PATH = $virglrendererPcdir
$version = & pkg-config --modversion virglrenderer
Write-Output "virglrenderer $version installed to $virglrendererPrefix"
