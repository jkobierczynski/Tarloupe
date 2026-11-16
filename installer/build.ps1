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
$WinFspSha256 = '073A70E00F77423E34BED98B86E600DEF93393BA5822204FAC57A29324DB9F7A'

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
        # The C runtime is linked in, so the programs start on machines
        # without the Visual C++ redistributable.
        $saved = $env:RUSTFLAGS
        $env:RUSTFLAGS = "$saved -C target-feature=+crt-static".Trim()
        try {
            cargo build --release --locked
            Check-Exit 'cargo build'
        }
        finally {
            $env:RUSTFLAGS = $saved
        }
    }
    $Bin = Join-Path $Root 'target\release'
    foreach ($exe in 'tarloupe.exe', 'tarloupew.exe') {
        $text = [Text.Encoding]::ASCII.GetString([IO.File]::ReadAllBytes((Join-Path $Bin $exe)))
        if ($text -match '(?i)vcruntime140') {
            throw "$exe needs the Visual C++ runtime; build it with -C target-feature=+crt-static"
        }
    }

    if (-not (Get-Command wix -ErrorAction SilentlyContinue)) {
        dotnet tool install --global wix --version $WixVersion
        Check-Exit 'installing WiX'
        $env:PATH += ";$env:USERPROFILE\.dotnet\tools"
    }
    $foundWix = (wix --version) -join ''
    if ($foundWix -notmatch '^5\.') {
        throw "WiX $foundWix is on PATH; this script needs WiX 5 (dotnet tool update --global wix --version $WixVersion)"
    }
    $Work = Join-Path $Root 'target\installer'
    New-Item -ItemType Directory -Force $Work, (Join-Path $Root 'dist') | Out-Null
    # The extensions come straight from their NuGet packages: `wix extension
    # add` has been seen to leave the Bal one "damaged".
    $Ext = @{}
    foreach ($name in 'WixToolset.Util.wixext', 'WixToolset.Bal.wixext') {
        $dir = Join-Path $Work "$name.$WixVersion"
        # The Bal package's DLL is WixToolset.BootstrapperApplications.wixext.dll.
        $find = { Get-ChildItem (Join-Path $dir 'wixext5') -Filter '*.wixext.dll' -ErrorAction SilentlyContinue | Select-Object -First 1 }
        $dll = & $find
        if (-not $dll) {
            # Missing, or left half there by an earlier run or a CI cache.
            if (Test-Path $dir) { Remove-Item -Recurse -Force $dir }
            $pkg = "$dir.zip"
            Invoke-WebRequest "https://www.nuget.org/api/v2/package/$name/$WixVersion" -OutFile $pkg
            Expand-Archive $pkg $dir
            Remove-Item $pkg
            $dll = & $find
        }
        if (-not $dll) { throw "no wixext5\*.wixext.dll in the $name $WixVersion package" }
        $Ext[$name] = $dll.FullName
    }

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
    if ($sig.Status -ne 'Valid' -or $sig.SignerCertificate.Subject -notmatch '^CN=NAVIMATICS LLC,') {
        throw "WinFsp installer is not validly signed by Navimatics ($($sig.Status))"
    }

    $Msi = Join-Path $Root "dist\tarloupe-$Version-x64.msi"
    $Setup = Join-Path $Root "dist\tarloupe-setup-$Version-x64.exe"
    wix build installer\Tarloupe.wxs -arch x64 `
        -d "Version=$Version" -d "Root=$Root" -d "BinDir=$Bin" -o $Msi
    Check-Exit 'building the MSI'
    wix build installer\Bundle.wxs -arch x64 `
        -ext $Ext['WixToolset.Util.wixext'] -ext $Ext['WixToolset.Bal.wixext'] `
        -d "Version=$Version" -d "Root=$Root" -d "WinFspMsi=$WinFspMsi" -d "TarloupeMsi=$Msi" -o $Setup
    Check-Exit 'building the setup program'
    Write-Host "Built $Msi"
    Write-Host "Built $Setup"
}
finally {
    Pop-Location
}
