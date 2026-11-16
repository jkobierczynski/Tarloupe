<#
.SYNOPSIS
  Builds the Tarloupe installers into dist\:
    tarloupe-<version>-x64.msi        Tarloupe only (WinFsp must be installed)
    tarloupe-setup-<version>-x64.exe  WinFsp, if missing, then Tarloupe

.DESCRIPTION
  Needs what building Tarloupe needs (Rust with MSVC, LLVM) plus the .NET
  SDK 8 or later for the WiX toolset, which it installs as a .NET tool if
  `wix` is missing. Downloads the pinned WinFsp release once and checks its
  hash and signature before bundling it.

.PARAMETER SkipCargo
  Use the programs already in target\release.
#>
param([switch]$SkipCargo)

$ErrorActionPreference = 'Stop'
$Root = Split-Path -Parent $PSScriptRoot

$WixVersion = '5.0.2'
$WinFspVersion = '2.1.25156'
$WinFspUrl = "https://github.com/winfsp/winfsp/releases/download/v2.1/winfsp-$WinFspVersion.msi"
$WinFspSha256 = ''

# Stops on a failed external command (PowerShell itself would carry on).
function Check-Exit([string]$What) {
    if ($LASTEXITCODE -ne 0) { throw "$What failed ($LASTEXITCODE)" }
}

Push-Location $Root
try {
    $m = Select-String -Path Cargo.toml -Pattern '^version = "([0-9.]+)"' | Select-Object -First 1
    if (-not $m) { throw 'no version in Cargo.toml' }
    $Version = $m.Matches[0].Groups[1].Value

    if (-not $SkipCargo) {
        cargo build --release --locked
        Check-Exit 'cargo build'
    }
    $Bin = Join-Path $Root 'target\release'

    if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
        dotnet tool install --global wix --version $WixVersion
        Check-Exit 'installing WiX'
        $env:PATH += ";$env:USERPROFILE\.dotnet\tools"
    }
    $have = (& wix extension list -g) -join "`n"
    foreach ($ext in 'WixToolset.Util.wixext', 'WixToolset.Bal.wixext') {
        if ($have -notmatch [regex]::Escape("$ext $WixVersion")) {
            wix extension add -g "$ext/$WixVersion"
            Check-Exit "adding $ext"
        }
    }

    $Work = Join-Path $Root 'target\installer'
    New-Item -ItemType Directory -Force $Work, (Join-Path $Root 'dist') | Out-Null
    $WinFspMsi = Join-Path $Work "winfsp-$WinFspVersion.msi"
    if (-not (Test-Path $WinFspMsi)) {
        Write-Host "Downloading $WinFspUrl"
        Invoke-WebRequest $WinFspUrl -OutFile "$WinFspMsi.part"
        Move-Item "$WinFspMsi.part" $WinFspMsi
    }
    $hash = (Get-FileHash $WinFspMsi -Algorithm SHA256).Hash
    Write-Host "WinFsp $WinFspVersion SHA-256 $hash"
    if ($WinFspSha256 -and $hash -ne $WinFspSha256) {
        Remove-Item $WinFspMsi
        throw "WinFsp download has SHA-256 $hash, expected $WinFspSha256"
    }
    $sig = Get-AuthenticodeSignature $WinFspMsi
    Write-Host "WinFsp signed by: $($sig.SignerCertificate.Subject) ($($sig.Status))"
    if ($sig.Status -ne 'Valid') { throw "WinFsp installer signature is $($sig.Status)" }

    $Msi = Join-Path $Root "dist\tarloupe-$Version-x64.msi"
    $Setup = Join-Path $Root "dist\tarloupe-setup-$Version-x64.exe"
    wix build installer\Tarloupe.wxs -arch x64 `
        -d "Version=$Version" -d "Root=$Root" -d "BinDir=$Bin" -o $Msi
    Check-Exit 'building the MSI'
    wix build installer\Bundle.wxs -arch x64 `
        -ext WixToolset.Util.wixext -ext WixToolset.Bal.wixext `
        -d "Version=$Version" -d "Root=$Root" -d "WinFspMsi=$WinFspMsi" -d "TarloupeMsi=$Msi" -o $Setup
    Check-Exit 'building the setup program'
    Write-Host "Built $Msi"
    Write-Host "Built $Setup"
}
finally {
    Pop-Location
}
