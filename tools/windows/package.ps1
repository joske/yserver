<#
.SYNOPSIS
Build the portable Windows zip: yserver-<version>-windows-<arch>.zip.

.DESCRIPTION
Installs the C libraries (build-env.ps1), builds yserver and xauth --release
for -Target, checks that neither exe needs a DLL beyond the system's, and zips
them with the XKB data, the launcher, README.txt and the licenses. Works on the
checkout it is part of, from any directory.

.PARAMETER Target
x86_64-pc-windows-msvc or aarch64-pc-windows-msvc. Default: the host's.

.PARAMETER XkbData
A directory holding xkb\ and COPYING.xkeyboard-config, as
tools/windows/build-xkb-data.sh makes it.

.PARAMETER VcpkgRoot
.PARAMETER Triplet
See build-env.ps1.

.PARAMETER OutDir
Where the zip goes. Default: target\dist.
#>
param(
    [ValidateSet('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc')]
    [string]$Target,
    [Parameter(Mandatory)][string]$XkbData,
    [string]$VcpkgRoot,
    [string]$Triplet,
    [string]$OutDir
)

$ErrorActionPreference = 'Stop'
Set-StrictMode -Version 3
$repo = (Resolve-Path "$PSScriptRoot\..\..").Path

function Invoke-Native {
    param([string]$Exe, [string[]]$Arguments)
    & $Exe @Arguments
    if ($LASTEXITCODE -ne 0) { throw "$Exe $($Arguments -join ' ') failed with exit code $LASTEXITCODE" }
}

if (-not $Target) {
    $Target = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'aarch64-pc-windows-msvc' } else { 'x86_64-pc-windows-msvc' }
}
$arch = $Target.Split('-')[0]
if (-not $OutDir) { $OutDir = "$repo\target\dist" }
$XkbData = (Resolve-Path $XkbData).Path
foreach ($need in 'xkb\rules\evdev', 'COPYING.xkeyboard-config') {
    if (-not (Test-Path "$XkbData\$need")) { throw "$XkbData has no $need" }
}

# Sets $installed, the vcpkg prefix.
. "$PSScriptRoot\build-env.ps1" -Target $Target -VcpkgRoot $VcpkgRoot -Triplet $Triplet

$meta = cargo metadata --format-version 1 --locked --manifest-path "$repo\Cargo.toml" --filter-platform $Target |
    ConvertFrom-Json
if ($LASTEXITCODE -ne 0) { throw 'cargo metadata failed' }
$version = ($meta.packages | Where-Object name -EQ 'yserver').version

Invoke-Native cargo @('build', '--release', '--locked', '--manifest-path', "$repo\Cargo.toml",
    '--target', $Target, '-p', 'yserver', '--bin', 'yserver', '-p', 'yserver-xauth', '--bin', 'xauth')
$exe = "$repo\target\$Target\release\yserver.exe"
# For ssh's X11 forwarding (XAuthLocation).
$xauth = "$repo\target\$Target\release\xauth.exe"

# Only system DLLs: the libraries above must be linked in, not imported,
# and so must the CRT (no vcruntime140.dll, no api-ms-win-crt-*).
$vswhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$hostDir = if ($env:PROCESSOR_ARCHITECTURE -eq 'ARM64') { 'Hostarm64' } else { 'Hostx64' }
$dumpbin = & $vswhere -latest -products * -find 'VC\Tools\MSVC\**\bin\**\dumpbin.exe' |
    Sort-Object { $_ -notmatch "\\$hostDir\\" } | Select-Object -First 1
if (-not $dumpbin) { throw 'dumpbin not found' }
$system = '^(api-ms-win-(?!crt-).*|kernel32|ntdll|user32|gdi32|advapi32|ws2_32|bcrypt|bcryptprimitives|' +
    'shell32|ole32|oleaut32|dwmapi|imm32|comctl32|shcore|uxtheme|userenv|secur32|crypt32|' +
    'winmm|version|shlwapi|combase|powrprof|setupapi|dbghelp|vulkan-1)\.dll$'
foreach ($bin in $exe, $xauth) {
    $leaf = Split-Path -Leaf $bin
    $dependents = & $dumpbin /nologo /dependents $bin |
        ForEach-Object { $_.Trim() } | Where-Object { $_ -match '\.dll$' }
    Write-Host "$leaf imports: $($dependents -join ', ')"
    $foreign = @($dependents | Where-Object { $_ -notmatch $system })
    if ($foreign) { throw "$leaf imports non-system DLLs: $($foreign -join ', ')" }
}

# Stage.
$name = "yserver-$version-windows-$arch"
$stage = "$repo\target\dist\$name"
if (Test-Path $stage) { Remove-Item -Recurse -Force $stage }
New-Item -ItemType Directory -Force "$stage\licenses" | Out-Null
Copy-Item $exe, $xauth $stage
Copy-Item -Recurse "$XkbData\xkb" "$stage\xkb"
function Copy-Crlf([string]$From, [string]$To) {
    $text = [IO.File]::ReadAllText($From) -replace "`r?`n", "`r`n"
    [IO.File]::WriteAllText($To, $text)
}
Copy-Crlf "$repo\tools\windows\start-yserver.cmd" "$stage\start-yserver.cmd"
Copy-Crlf "$repo\tools\windows\README.txt" "$stage\README.txt"
Copy-Crlf "$repo\LICENSE" "$stage\LICENSE.txt"
Copy-Crlf "$XkbData\COPYING.xkeyboard-config" "$stage\licenses\xkeyboard-config.txt"

# The vcpkg ports linked into the exe: libxkbcommon, freetype and whichever
# of freetype's optional dependencies this triplet built it with.
foreach ($port in 'libxkbcommon', 'freetype', 'zlib', 'libpng', 'bzip2', 'brotli') {
    $copyright = "$installed\share\$port\copyright"
    if (Test-Path $copyright) { Copy-Crlf $copyright "$stage\licenses\$port.txt" }
    elseif ($port -in 'libxkbcommon', 'freetype') { throw "no license for $port in $installed" }
}

# The Rust crates linked into the exes: the normal (not build or dev)
# dependencies of yserver for this target (xauth's are a subset), with
# their license files.
$nodes = @{}
foreach ($node in $meta.resolve.nodes) { $nodes[$node.id] = $node }
$packages = @{}
foreach ($pkg in $meta.packages) { $packages[$pkg.id] = $pkg }
$members = @($meta.workspace_members)
$root = ($meta.packages | Where-Object name -EQ 'yserver').id
$seen = [Collections.Generic.HashSet[string]]::new()
$queue = [Collections.Generic.Queue[string]]::new()
$queue.Enqueue($root)
while ($queue.Count) {
    $id = $queue.Dequeue()
    if (-not $seen.Add($id)) { continue }
    foreach ($dep in $nodes[$id].deps) {
        if ($dep.dep_kinds | Where-Object { $null -eq $_.kind }) { $queue.Enqueue($dep.pkg) }
    }
}
$crates = [Text.StringBuilder]::new()
[void]$crates.AppendLine('The Rust crates in yserver.exe and xauth.exe, with their licenses.')
foreach ($pkg in $seen | Where-Object { $_ -notin $members } | ForEach-Object { $packages[$_] } |
    Sort-Object name, version) {
    $dir = Split-Path $pkg.manifest_path
    [void]$crates.AppendLine().AppendLine('=' * 78)
    [void]$crates.AppendLine("$($pkg.name) $($pkg.version): $($pkg.license)")
    [void]$crates.AppendLine('=' * 78)
    $files = @(Get-ChildItem $dir -File | Where-Object Name -Match '^(LICEN[CS]E|COPYING|NOTICE)' |
            Sort-Object Name)
    if ($pkg.license_file) { $files += Get-Item (Join-Path $dir $pkg.license_file) }
    if (-not $files) { [void]$crates.AppendLine("(no license file in the crate; SPDX: $($pkg.license))") }
    foreach ($f in $files | Sort-Object FullName -Unique) {
        [void]$crates.AppendLine("--- $($f.Name)").AppendLine([IO.File]::ReadAllText($f.FullName).TrimEnd())
    }
}
[IO.File]::WriteAllText("$stage\licenses\rust-crates.txt", ($crates.ToString() -replace "`r?`n", "`r`n"))

$zip = "$OutDir\$name.zip"
New-Item -ItemType Directory -Force $OutDir | Out-Null
if (Test-Path $zip) { Remove-Item $zip }
Compress-Archive -Path $stage -DestinationPath $zip -CompressionLevel Optimal
Write-Host ("{0}: {1:N1} MB" -f $zip, ((Get-Item $zip).Length / 1MB))
if ($env:GITHUB_OUTPUT) { Add-Content $env:GITHUB_OUTPUT "zip=$zip" }
