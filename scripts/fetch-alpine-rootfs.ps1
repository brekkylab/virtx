# Download and unpack an Alpine minirootfs to use as the guest root filesystem,
# in the shape libkrun's Windows virtio-fs passthrough expects.
#
# The POSIX script next to this one cannot do the job on a Windows host. Two
# things a Linux rootfs needs have no native NTFS equivalent, and the
# passthrough reads each from a specific place:
#
#   symlinks    `readlink()` is served from an IO_REPARSE_TAG_LX_SYMLINK
#               reparse point -- the WSL representation, which stores the
#               target verbatim as a Linux path. A native Windows symlink
#               (IO_REPARSE_TAG_SYMLINK) cannot hold `/bin/busybox`: Windows
#               resolves it against the current drive. So the tarball is
#               unpacked from inside WSL, which creates LX symlinks on drvfs.
#
#   mode bits   NTFS has no execute bit. `file_info_to_stat64` reports 0o644
#               for every regular file and 0o755 for every directory, and only
#               an alternate data stream named `user.containers.override_stat`
#               -- holding `uid:gid:0mode` -- overrides that. Without it
#               /bin/busybox is not executable and init dies with EACCES.
#
# Entries whose mode already matches what the passthrough assumes get no
# stream, so the rootfs carries a few dozen of these rather than one per file.
#
#   .\scripts\fetch-alpine-rootfs.ps1
#   .\scripts\fetch-alpine-rootfs.ps1 -Distro Ubuntu-24.04
#
[CmdletBinding()]
param(
    [string] $Version = $(if ($env:ALPINE_VERSION) { $env:ALPINE_VERSION } else { '3.24.2' }),
    # WSL distribution used to unpack the tarball. Defaults to the default one.
    [string] $Distro
)

$ErrorActionPreference = 'Stop'

$STREAM = 'user.containers.override_stat'
# What file_info_to_stat64() reports with no override stream present.
$DEFAULT_FILE_MODE = '644'
$DEFAULT_DIR_MODE = '755'

$root = Split-Path -Parent $PSScriptRoot
$dest = Join-Path $root 'data\rootfs\alpine'

if (Test-Path (Join-Path $dest 'bin\busybox')) {
    Write-Host "rootfs already present at $dest"
    exit 0
}

# WHP is x86_64 only, so there is no architecture to choose here.
$parts = $Version -split '\.'
$branch = "v$($parts[0]).$($parts[1])"
$tarball = "alpine-minirootfs-$Version-x86_64.tar.gz"
$url = "https://dl-cdn.alpinelinux.org/alpine/$branch/releases/x86_64/$tarball"

$wslArgs = @()
if ($Distro) { $wslArgs += @('-d', $Distro) }

# Translate a Windows path to the drvfs mount WSL sees it under.
function ConvertTo-WslPath([string] $Path) {
    $full = [IO.Path]::GetFullPath($Path)
    $drive = $full.Substring(0, 1).ToLower()
    '/mnt/' + $drive + $full.Substring(2).Replace('\', '/')
}

$downloads = Join-Path $root 'data'
New-Item -ItemType Directory -Force -Path $downloads | Out-Null
$archive = Join-Path $downloads $tarball

if (-not (Test-Path $archive)) {
    Write-Host "fetching $url"
    Invoke-WebRequest -Uri $url -OutFile $archive -UseBasicParsing
}

# Unpack from inside WSL so the symlinks land as LX reparse points.
Write-Host "unpacking to $dest"
New-Item -ItemType Directory -Force -Path $dest | Out-Null
$unpack = "set -eu; tar -xzf '$(ConvertTo-WslPath $archive)' -C '$(ConvertTo-WslPath $dest)'"
& wsl @wslArgs -- sh -c $unpack
if ($LASTEXITCODE -ne 0) { throw "tar failed in WSL (exit $LASTEXITCODE)" }

# The minirootfs ships no resolv.conf. With TSI there is no network interface
# in the guest and nothing to configure one, so point at a public resolver.
Set-Content -Path (Join-Path $dest 'etc\resolv.conf') -Encoding Ascii -NoNewline `
    -Value "nameserver 1.1.1.1`nnameserver 8.8.8.8`n"

# Read the modes back out of the tarball rather than off disk -- drvfs reports
# a synthesised mode for every file, so the unpacked tree no longer knows them.
$manifest = @'
import sys, tarfile
with tarfile.open(sys.argv[1]) as tf:
    for m in tf:
        if m.isfile():
            kind = "f"
        elif m.isdir():
            kind = "d"
        else:
            continue
        print("%s\t%04o\t%d\t%d\t%s" % (kind, m.mode, m.uid, m.gid, m.name))
'@
# Via a file, not `python3 -c`: wsl.exe rewrites quotes and backslashes in the
# arguments it forwards, which mangles the script.
$manifestPy = Join-Path $downloads 'tar-modes.py'
Set-Content -LiteralPath $manifestPy -Value $manifest -Encoding Ascii
$entries = & wsl @wslArgs -- python3 (ConvertTo-WslPath $manifestPy) (ConvertTo-WslPath $archive)
if ($LASTEXITCODE -ne 0) { throw "reading modes from the tarball failed (exit $LASTEXITCODE)" }

$written = 0
foreach ($line in $entries) {
    if (-not $line) { continue }
    $kind, $mode, $uid, $gid, $name = $line -split "`t", 5

    $default = if ($kind -eq 'd') { $DEFAULT_DIR_MODE } else { $DEFAULT_FILE_MODE }
    if ($mode -eq "0$default" -and $uid -eq '0' -and $gid -eq '0') { continue }

    $path = Join-Path $dest ($name -replace '/', '\')
    if (-not (Test-Path -LiteralPath $path)) { continue }

    Set-Content -LiteralPath $path -Stream $STREAM -Encoding Ascii -NoNewline `
        -Value "${uid}:${gid}:$mode"
    $written++
}

Write-Host "unpacked to $dest ($written override_stat streams)"
