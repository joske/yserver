<#
.SYNOPSIS
Set up what building yserver on Windows needs: the C libraries from vcpkg
(static, on the static CRT, release only), glslc from the Vulkan SDK, and the
environment for them: PKG_CONFIG, PKG_CONFIG_PATH, PKG_CONFIG_ALL_STATIC,
GLSLC and RUSTFLAGS, with +crt-static so the exe needs no Visual C++ runtime. Also into $env:GITHUB_ENV on GitHub Actions.

.PARAMETER Target
x86_64-pc-windows-msvc or aarch64-pc-windows-msvc. Default: the host's.

.PARAMETER VcpkgRoot
Default: $env:VCPKG_ROOT, else $env:VCPKG_INSTALLATION_ROOT, else C:\vcpkg.

.PARAMETER Triplet
Default: <arch>-windows-static-release, from tools/windows/vcpkg-triplets;
also the host triplet, so vcpkg's host tools are release-only too.
#>
param(
    [ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')]
    [string]$Target,
    [string]$VcpkgRoot,
    [string]$Triplet
)

$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'  # Invoke-WebRequest crawls with it
Set-StrictMode -Version 3
$repo = (Resolve-Path "$PSScriptRoot\..\..").Path

if (-not $Target) {
    $Target = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
}
$vcpkgArch = @{ 'x86_64' = 'x64'; 'aarch64' = 'arm64' }[$Target.Split('-')[0]]
if (-not $Triplet) { $Triplet = "$vcpkgArch-windows-static-release" }
if (-not $VcpkgRoot) {
    $VcpkgRoot = @($env:VCPKG_ROOT, $env:VCPKG_INSTALLATION_ROOT, 'C:\vcpkg') |
        Where-Object { $_ -and (Test-Path "$_\vcpkg.exe") } | Select-Object -First 1
}
if (-not $VcpkgRoot) { throw 'no vcpkg: pass -VcpkgRoot or set VCPKG_ROOT' }

# glslc runs at build time, so it is the host's. The SDK installer is the
# only Windows build LunarG ships: install it headless into a scratch root,
# keep glslc.exe in target\tools, and purge the rest. Note that installing
# the SDK also updates the system's Vulkan loader.
$sdkVersion = '1.4.357.0'
$sdk = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') {
    @{ Dir = 'warm'; File = "vulkansdk-windows-ARM64-$sdkVersion.exe"
        Sha256 = 'c10f18a9085018f66e1f50bd60623f17b7081faca165248de54f78728120f334' }
} else {
    @{ Dir = 'windows'; File = "vulkansdk-windows-X64-$sdkVersion.exe"
        Sha256 = '81f474711e9042f4cd22b31b2f7a8870db2e428b21586fb43dd80150be97310d' }
}
$glslcDir = "$repo\target\tools\glslc-$sdkVersion-$($env:PROCESSOR_ARCHITECTURE)"
if (-not (Test-Path "$glslcDir\glslc.exe")) {
    $scratch = Join-Path ([IO.Path]::GetTempPath()) "yserver-vulkansdk-$PID"
    New-Item -ItemType Directory -Force $scratch | Out-Null
    try {
        $installer = "$scratch\$($sdk.File)"
        Invoke-WebRequest -UseBasicParsing -OutFile $installer `
            "https://sdk.lunarg.com/sdk/download/$sdkVersion/$($sdk.Dir)/$($sdk.File)"
        $hash = (Get-FileHash -Algorithm SHA256 $installer).Hash
        if ($hash -ne $sdk.Sha256) { throw "$($sdk.File): sha256 $hash, expected $($sdk.Sha256)" }
        & $installer --root "$scratch\sdk" --accept-licenses --default-answer --confirm-command install | Out-Null
        if ($LASTEXITCODE -ne 0) { throw "Vulkan SDK install failed with exit code $LASTEXITCODE" }
        New-Item -ItemType Directory -Force $glslcDir | Out-Null
        Copy-Item "$scratch\sdk\Bin\glslc.exe" $glslcDir
        & "$scratch\sdk\maintenancetool.exe" purge --accept-licenses --default-answer --confirm-command | Out-Null
    } finally {
        Remove-Item -Recurse -Force $scratch -ErrorAction SilentlyContinue
    }
}

# pkgconf is a build tool; the rest is linked in. libxkbcommon is the
# overlay port in vcpkg-ports, without xkbcommon-x11 and its X11 deps.
$env:VCPKG_DEFAULT_HOST_TRIPLET = $Triplet
& "$VcpkgRoot\vcpkg.exe" install libxkbcommon freetype pkgconf "--triplet=$Triplet" `
    "--overlay-triplets=$PSScriptRoot\vcpkg-triplets" "--overlay-ports=$PSScriptRoot\vcpkg-ports" `
    --clean-after-build
if ($LASTEXITCODE -ne 0) { throw "vcpkg install failed with exit code $LASTEXITCODE" }

$installed = "$VcpkgRoot\installed\$Triplet"
$vars = [ordered]@{
    PKG_CONFIG            = "$installed\tools\pkgconf\pkgconf.exe"
    PKG_CONFIG_PATH       = "$installed\lib\pkgconfig"
    # freetype's own dependencies (zlib, libpng, ...) are in its Libs.private.
    PKG_CONFIG_ALL_STATIC = '1'
    GLSLC                 = "$glslcDir\glslc.exe"
    # The static CRT, as the vcpkg libraries are built against: mixing the
    # two is LNK4098 at best. Here rather than in .cargo/config.toml, which
    # RUSTFLAGS would override and which would break DLL-CRT vcpkg builds.
    # The xkbcommon crate's `#[link]` is dylib-kind, so it calls through
    # __imp_ pointers that a static library lacks; a static `-l` overrides
    # that kind in every crate.
    RUSTFLAGS             = (@($env:RUSTFLAGS,
            "-C target-feature=+crt-static -L native=$installed\lib -l static=xkbcommon") |
            Where-Object { $_ }) -join ' '
}
foreach ($kv in $vars.GetEnumerator()) {
    Set-Item "env:$($kv.Key)" $kv.Value
    if ($env:GITHUB_ENV) { Add-Content $env:GITHUB_ENV "$($kv.Key)=$($kv.Value)" }
}
