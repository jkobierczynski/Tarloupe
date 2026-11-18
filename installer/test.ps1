<#
.SYNOPSIS
  Installs a built tarloupe-setup-*.exe quietly, checks what it put where
  (including that the bundled WinFsp mounts an archive), uninstalls it and
  checks that Tarloupe is gone and WinFsp stays. Changes the machine: CI
  only. Needs administrator rights.
#>
param([Parameter(Mandatory)][string]$Setup)

$ErrorActionPreference = 'Stop'
$Temp = if ($env:RUNNER_TEMP) { $env:RUNNER_TEMP } else { $env:TEMP }

function Check([bool]$ok, [string]$what) {
    if (-not $ok) { throw "FAILED: $what" }
    Write-Host "ok: $what"
}

function Setup([string[]]$Arguments, [string]$Log) {
    $p = Start-Process $Setup -ArgumentList ($Arguments + @('/log', $Log)) -Wait -PassThru
    if ($p.ExitCode -ne 0) {
        Get-ChildItem $Temp -Filter 'setup*.log' | ForEach-Object { Get-Content $_.FullName -Tail 60 }
        throw "$Setup $Arguments exited with $($p.ExitCode)"
    }
}

$Dir = Join-Path $env:ProgramFiles 'Tarloupe'
$WinFspKey = 'HKLM:\SOFTWARE\WOW6432Node\WinFsp'
$VerbKeys = '.tar', '.tgz', '.gz', '.txz', '.xz', '.tzst', '.zst', '.tbz2', '.tbz', '.bz2' | ForEach-Object {
    "Registry::HKEY_LOCAL_MACHINE\Software\Classes\SystemFileAssociations\$_\shell\Tarloupe.Browse"
}
function OnPath {
    $p = [Environment]::GetEnvironmentVariable('PATH', 'Machine') -split ';'
    [bool]($p | Where-Object { $_.TrimEnd('\') -ieq $Dir })
}

Setup @('/quiet', '/norestart') (Join-Path $Temp 'setup-install.log')
Check (Test-Path "$Dir\tarloupe.exe") 'tarloupe.exe installed'
Check (Test-Path "$Dir\tarloupew.exe") 'tarloupew.exe installed'
Check ([bool](Get-ItemProperty $WinFspKey -ErrorAction SilentlyContinue).InstallDir) 'WinFsp installed'
Check (OnPath) 'program folder on PATH'
foreach ($k in $VerbKeys) {
    $cmd = (Get-ItemProperty "$k\command").'(default)'
    Check ($cmd -eq "`"$Dir\tarloupew.exe`" `"%1`"") "right-click item ${k}: $cmd"
}
Check ((Get-ItemProperty $VerbKeys[2]).AppliesTo -eq 'System.FileName:~"*.tar.gz"') '.gz item only for .tar.gz'
& "$Dir\tarloupe.exe" --version
Check ($LASTEXITCODE -eq 0) 'tarloupe --version runs'

# The bundled WinFsp works: mount a small archive and read a file back.
$Work = Join-Path $Temp 'tarloupe-smoke'
New-Item -ItemType Directory -Force $Work | Out-Null
Set-Content -NoNewline -Path "$Work\hello.txt" -Value 'hello from the installer test'
& tar -cf "$Work\smoke.tar" -C $Work hello.txt
Check ($LASTEXITCODE -eq 0) 'made a test archive'
$mount = Start-Process "$Dir\tarloupe.exe" -ArgumentList 'mount', "`"$Work\smoke.tar`"", 'T:' `
    -PassThru -NoNewWindow -RedirectStandardError "$Work\mount.err"
try {
    $deadline = (Get-Date).AddSeconds(120)
    while (-not (Test-Path 'T:\hello.txt') -and (Get-Date) -lt $deadline -and -not $mount.HasExited) {
        Start-Sleep -Milliseconds 500
    }
    if (-not (Test-Path 'T:\hello.txt')) { Get-Content "$Work\mount.err" -ErrorAction SilentlyContinue }
    Check ((Get-Content -Raw 'T:\hello.txt') -eq 'hello from the installer test') 'mounted archive reads back'
}
finally {
    if (-not $mount.HasExited) { Stop-Process -Id $mount.Id -Force }
}
Start-Sleep -Seconds 2

Setup @('/uninstall', '/quiet', '/norestart') (Join-Path $Temp 'setup-uninstall.log')
Check (-not (Test-Path "$Dir\tarloupe.exe")) 'programs removed'
foreach ($k in $VerbKeys) { Check (-not (Test-Path $k)) "right-click item removed: $k" }
Check (-not (OnPath)) 'program folder off PATH'
Check ([bool](Get-ItemProperty $WinFspKey -ErrorAction SilentlyContinue).InstallDir) 'WinFsp left installed'
