# Cargo runner for x86_64-pc-windows-msvc, registered in .cargo/config.toml.
#
# It does the one thing that is mandatory on Windows: puts `data/krunfw/` on
# PATH. libkrun resolves the guest kernel with `LoadLibrary("libkrunfw.dll")`,
# by leaf name, and no release ships that DLL -- it is built by
# `scripts/build-libkrunfw-windows.sh` into `data/krunfw/`, which is not a
# directory the loader searches.
#
# Running the built binary directly, without the runner, fails with
# `Error: FileNotFound` unless the DLL is next to the executable or on PATH.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true, Position = 0)] [string] $Binary,
    [Parameter(ValueFromRemainingArguments = $true)] [string[]] $BinaryArgs
)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent $PSScriptRoot
$krunfw = Join-Path $root 'data\krunfw'

if (-not (Test-Path (Join-Path $krunfw 'libkrunfw.dll'))) {
    Write-Error "no libkrunfw.dll in $krunfw; run scripts/build-libkrunfw-windows.sh first"
}

$env:PATH = "$krunfw;$env:PATH"

# The `gpu` feature links virglrenderer, which loads as a DLL and pulls epoxy in
# with it. Both are built into data/ by scripts/build-virglrenderer-windows.ps1
# and neither is anywhere the loader looks. Harmless when the feature is off.
foreach ($dir in @((Join-Path $root 'build\epoxy\bin'))) {
    if (Test-Path $dir) { $env:PATH = "$dir;$env:PATH" }
}

& $Binary @BinaryArgs
exit $LASTEXITCODE
