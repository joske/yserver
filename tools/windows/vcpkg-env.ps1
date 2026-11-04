<#
.SYNOPSIS
Install the C libraries yserver links with vcpkg, and set the build
environment for them: PKG_CONFIG, PKG_CONFIG_PATH, PKG_CONFIG_ALL_STATIC and
GLSLC. Also into $env:GITHUB_ENV on GitHub Actions.

.PARAMETER Target
x86_64-pc-windows-msvc or aarch64-pc-windows-msvc. Default: the host's.

.PARAMETER VcpkgRoot
Default: $env:VCPKG_ROOT, else $env:VCPKG_INSTALLATION_ROOT, else C:\vcpkg.

.PARAMETER Triplet
Default: <arch>-windows-static-md-release (static libraries on the DLL CRT,
release only), from tools/windows/vcpkg-triplets.
#>
param(
    [ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')]
    [string]$Target,
    [string]$VcpkgRoot,
    [string]$Triplet
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3

if (-not $Target) {
    $Target = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
}
$vcpkgArch = @{ 'x86_64' = 'x64'; 'aarch64' = 'arm64' }[$Target.Split('-')[0]]
if (-not $Triplet) { $Triplet = "$vcpkgArch-windows-static-md-release" }
if (-not $VcpkgRoot) {
    $VcpkgRoot = @($env:VCPKG_ROOT, $env:VCPKG_INSTALLATION_ROOT, 'C:\vcpkg') |
        Where-Object { $_ -and (Test-Path "$_\vcpkg.exe") } | Select-Object -First 1
}
if (-not $VcpkgRoot) { throw 'no vcpkg: pass -VcpkgRoot or set VCPKG_ROOT' }

# pkgconf and shaderc (for glslc) are build tools; the rest is linked in.
& "$VcpkgRoot\vcpkg.exe" install libxkbcommon freetype pkgconf shaderc "--triplet=$Triplet" `
    "--overlay-triplets=$PSScriptRoot\vcpkg-triplets" --clean-after-build
if ($LASTEXITCODE -ne 0) { throw "vcpkg install failed with exit code $LASTEXITCODE" }

$installed = "$VcpkgRoot\installed\$Triplet"
$vars = [ordered]@{
    PKG_CONFIG            = "$installed\tools\pkgconf\pkgconf.exe"
    PKG_CONFIG_PATH       = "$installed\lib\pkgconfig"
    # freetype's own dependencies (zlib, libpng, ...) are in its Libs.private.
    PKG_CONFIG_ALL_STATIC = '1'
    GLSLC                 = "$installed\tools\shaderc\glslc.exe"
}
foreach ($kv in $vars.GetEnumerator()) {
    Set-Item "env:$($kv.Key)" $kv.Value
    if ($env:GITHUB_ENV) { Add-Content $env:GITHUB_ENV "$($kv.Key)=$($kv.Value)" }
}
