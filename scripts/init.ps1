[CmdletBinding()]
param([switch] $Rebuild)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot

Write-Output 'Applying patches'

function Apply-Patch([string] $submodule, [string] $patchFile) {
    $dir = Join-Path $root "3rdparty\$submodule"

    if (-not (Test-Path (Join-Path $dir '.git'))) {
        Write-Error "$submodule is not checked out; run: git submodule update --init"
    }

    & git -C $dir apply --reverse --check $patchFile 2>$null
    if ($LASTEXITCODE -eq 0) {
        Write-Output "  ${submodule}: already patched"
        return
    }

    & git -C $dir apply --check $patchFile 2>$null
    if ($LASTEXITCODE -ne 0) {
        Write-Error ("  ${submodule}: patch does not apply -- the submodule has moved or`n" +
            "  has other local changes. Reset it or refresh $patchFile.")
    }

    & git -C $dir apply $patchFile
    if ($LASTEXITCODE -ne 0) { Write-Error "  ${submodule}: git apply failed" }
    Write-Output "  ${submodule}: applied $(Split-Path -Leaf $patchFile)"
}

function Invoke-Checked([string] $what, [scriptblock] $block) {
    & $block
    if ($LASTEXITCODE -ne 0) { Write-Error "$what failed ($LASTEXITCODE)" }
}

$patches = Join-Path $root '3rdparty\_patches'

Get-ChildItem -Path $patches -Filter '*.patch' | Sort-Object Name | ForEach-Object {
    Apply-Patch ($_.Name -split '-', 2)[0] $_.FullName
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
if (-not $env:VULKAN_SDK) { Write-Error 'VULKAN_SDK is not set; install the Vulkan SDK' }
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
    $python = (Get-Command python.exe -ErrorAction SilentlyContinue).Source
    if (-not $python -or -not (& $python --version 2>$null)) {
        Write-Error 'no usable python.exe on PATH (the Microsoft Store stub does not count)'
    }
    Write-Output 'Creating the meson venv'
    Invoke-Checked 'venv' { & $python -m venv $venv }
    Invoke-Checked 'pip install' {
        & (Join-Path $venv 'Scripts\python.exe') -m pip install --quiet --upgrade pip meson pyyaml mako
    }
}
& (Join-Path $venv 'Scripts\python.exe') -c 'import yaml, mako' 2>$null
if ($LASTEXITCODE -ne 0) {
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

$sdk = $env:VULKAN_SDK -replace '\\','/'
$sdkVersion = Split-Path -Leaf $env:VULKAN_SDK
@"
Name: vulkan
Description: Vulkan SDK loader
Version: $sdkVersion
Cflags: -I$sdk/Include
Libs: -L$sdk/Lib -lvulkan-1
"@ | Set-Content -Encoding ascii (Join-Path $pcdir 'vulkan.pc')

Write-Output 'Setting up libepoxy'

if (Test-Path (Join-Path $epoxyBuild 'build.ninja')) { $reconfigure = @('--reconfigure') } else { $reconfigure = @() }
Invoke-Checked 'libepoxy configure' {
    & $meson setup @reconfigure $epoxyBuild $epoxySrc --prefix $epoxyPrefix --buildtype release `
        --native-file $native -Dtests=false -Ddocs=false | Out-Null
}
Invoke-Checked 'libepoxy build' { & $meson compile -C $epoxyBuild | Out-Null }
Invoke-Checked 'libepoxy install' { & $meson install -C $epoxyBuild | Out-Null }

Write-Output 'Setting up virglrenderer'

$env:PKG_CONFIG_PATH = "$pcdir;$(Join-Path $epoxyPrefix 'lib\pkgconfig')"

if (Test-Path (Join-Path $virglrendererBuild 'build.ninja')) { $reconfigure = @('--reconfigure') } else { $reconfigure = @() }
Write-Output "Configuring virglrenderer ($(& git -C $virglrendererSrc rev-parse --short HEAD))"
Invoke-Checked 'virglrenderer configure' {
    & $meson setup @reconfigure $virglrendererBuild $virglrendererSrc --prefix $virglrendererPrefix `
        --buildtype release --native-file $native `
        -Dvenus=true -Drender-server-mode=none -Dvulkan-dload=false -Dplatforms= `
        -Ddefault_library=static | Out-Null
}

Write-Output 'Compiling virglrenderer'
Invoke-Checked 'virglrenderer build' { & $meson compile -C $virglrendererBuild | Out-Null }
Invoke-Checked 'virglrenderer install' { & $meson install -C $virglrendererBuild | Out-Null }

$virglrendererPcdir = Join-Path $virglrendererPrefix 'lib\pkgconfig'
Copy-Item (Join-Path $pcdir 'vulkan.pc') (Join-Path $virglrendererPcdir 'vulkan.pc') -Force
Copy-Item (Join-Path $epoxyPrefix 'lib\pkgconfig\epoxy.pc') (Join-Path $virglrendererPcdir 'epoxy.pc') -Force

$env:PKG_CONFIG_PATH = $virglrendererPcdir
$version = & pkg-config --modversion virglrenderer
Write-Output "virglrenderer $version installed to $virglrendererPrefix"
Write-Output "epoxy-0.dll ($(Join-Path $epoxyPrefix 'bin')) must be beside the binary or on PATH"
