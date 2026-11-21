# The GPU libraries the servers' `gpu` feature links, each built by its own script in 3rdparty\_scripts\,
# in the order one links another. What is given here is given to each: `-Rebuild`.
[CmdletBinding()]
param([switch] $Rebuild)

$ErrorActionPreference = 'Stop'

$root = Split-Path -Parent (Split-Path -Parent $PSScriptRoot)

& (Join-Path $root '3rdparty\_scripts\build-virglrenderer.ps1') -Rebuild:$Rebuild
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }
