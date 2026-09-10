# Stage exact WebCodex runtime bytes for a native Windows Tauri Desktop bundle.
#
# This helper consumes already-built release binaries, verifies one exact
# source/version/build identity, copies them into an ignored generated tree,
# proves the copies are byte-for-byte identical, and writes a Tauri config
# overlay that maps only those staged files into `webcodex-runtime`.
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][string]$BinDir,
    [Parameter(Mandatory = $true)][string]$Version,
    [Parameter(Mandatory = $true)][string]$SourceSha,
    [Parameter(Mandatory = $true)][Int64]$BuiltAt,
    [Parameter(Mandatory = $true)][string]$OutputDir
)

$ErrorActionPreference = "Stop"

if ($Version -notmatch '^[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$') {
    throw "invalid Desktop bundle version '$Version'"
}
if ($SourceSha -notmatch '^[0-9A-Fa-f]{40}$') {
    throw "SourceSha must be one exact 40-hex Git commit"
}
if ($BuiltAt -le 0) {
    throw "BuiltAt must be a positive Unix timestamp"
}

$BinDir = [System.IO.Path]::GetFullPath($BinDir)
$OutputDir = [System.IO.Path]::GetFullPath($OutputDir)
if (Test-Path -LiteralPath $OutputDir) {
    throw "Desktop bundle output already exists: $OutputDir"
}

$runtimeDir = Join-Path $OutputDir "resources\webcodex-runtime"
New-Item -ItemType Directory -Force -Path $runtimeDir | Out-Null
$shortSource = $SourceSha.Substring(0, 12).ToLowerInvariant()
$binaryNames = @("webcodex", "webcodex-server", "webcodex-runner")
$resourceMap = [ordered]@{}
$fileMetadata = [ordered]@{}

try {
    foreach ($name in $binaryNames) {
        $source = Join-Path $BinDir "$name.exe"
        if (-not (Test-Path -LiteralPath $source -PathType Leaf)) {
            throw "missing Desktop runtime binary: $source"
        }
        $sourceItem = Get-Item -LiteralPath $source
        if (($sourceItem.Attributes -band [System.IO.FileAttributes]::ReparsePoint) -ne 0) {
            throw "Desktop runtime binary must be a regular non-reparse file: $source"
        }

        # Capture the native process to completion before selecting the first
        # output line. Piping the native process directly into Select-Object
        # -First can stop the upstream process early on PowerShell 7 and leave
        # a non-zero $LASTEXITCODE even though the version line was emitted.
        $output = @(& $source --version)
        $exitCode = $LASTEXITCODE
        $line = @($output | Select-Object -First 1)
        if ($exitCode -ne 0 -or $line.Count -eq 0 -or -not $line[0]) {
            throw "$name.exe --version failed while staging Desktop resources (exit code $exitCode)"
        }
        $line = $line[0].TrimEnd()
        $expected = "$name $Version (commit $shortSource, dirty=false, built_at=$BuiltAt)"
        if ($line -ne $expected) {
            throw "unexpected $name.exe identity: '$line' (expected '$expected')"
        }

        $sourceHash = (Get-FileHash -LiteralPath $source -Algorithm SHA256).Hash.ToLowerInvariant()
        $destination = Join-Path $runtimeDir "$name.exe"
        Copy-Item -LiteralPath $source -Destination $destination
        $destinationItem = Get-Item -LiteralPath $destination
        $destinationHash = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($destinationItem.Length -ne $sourceItem.Length -or $destinationHash -ne $sourceHash) {
            throw "staged Desktop runtime byte verification failed for $name.exe"
        }

        $resourceMap[[System.IO.Path]::GetFullPath($destination)] = "webcodex-runtime/$name.exe"
        $fileMetadata[$name] = [ordered]@{
            filename = "$name.exe"
            size = [Int64]$destinationItem.Length
            sha256 = $destinationHash
        }
    }

    $overlay = [ordered]@{
        version = $Version
        bundle = [ordered]@{
            active = $true
            targets = @("nsis")
            resources = $resourceMap
            windows = [ordered]@{
                nsis = [ordered]@{
                    installMode = "currentUser"
                }
            }
        }
    }
    $overlayPath = Join-Path $OutputDir "tauri.bundle.conf.json"
    $utf8 = [System.Text.UTF8Encoding]::new($false)
    [System.IO.File]::WriteAllText($overlayPath, ($overlay | ConvertTo-Json -Depth 8) + "`n", $utf8)

    $metadata = [ordered]@{
        schema_version = 1
        version = $Version
        source_sha = $SourceSha.ToLowerInvariant()
        built_at = $BuiltAt
        resource_dir = "resources/webcodex-runtime"
        files = $fileMetadata
    }
    $metadataPath = Join-Path $OutputDir "desktop-bundle.json"
    [System.IO.File]::WriteAllText($metadataPath, ($metadata | ConvertTo-Json -Depth 8) + "`n", $utf8)

    Write-Output "Desktop runtime staged from exact source $($SourceSha.ToLowerInvariant())"
    Write-Output "Tauri config overlay: $overlayPath"
    Write-Output "Runtime resources: $runtimeDir"
} catch {
    if (Test-Path -LiteralPath $OutputDir) {
        Remove-Item -LiteralPath $OutputDir -Recurse -Force
    }
    throw
}
