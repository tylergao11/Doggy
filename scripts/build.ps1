# Build Doggy and install under ~/.Doggy (not ~/.grok) + ~/.local/bin.
#
# Usage (from repo root):
#   .\scripts\build.ps1           # release (default) + install
#   .\scripts\build.ps1 -Debug    # debug binary
#   .\scripts\build.ps1 -Run      # build then launch doggy
#   .\scripts\build.ps1 -NoInstall

param(
    [switch]$Debug,
    [switch]$Run,
    [switch]$NoInstall
)

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $Root

$protoc = Join-Path $Root ".tools\protoc\bin\protoc.exe"
if (Test-Path $protoc) {
    $env:PROTOC = $protoc
}

$pkg = "xai-grok-pager-bin"
if ($Debug) {
    Write-Host "[doggy] cargo build -p $pkg (debug) ..."
    cargo build -p $pkg
    $exe = Join-Path $Root "target\debug\xai-grok-pager.exe"
} else {
    Write-Host "[doggy] cargo build -p $pkg --release ..."
    cargo build -p $pkg --release
    $exe = Join-Path $Root "target\release\xai-grok-pager.exe"
}

if (-not (Test-Path $exe)) {
    throw "Build finished but binary missing: $exe"
}

$sizeMb = [math]::Round((Get-Item $exe).Length / 1MB, 1)
Write-Host "[doggy] OK: $exe ($sizeMb MB)"

function Install-DoggyTo([string]$destDir) {
    if (-not (Test-Path $destDir)) {
        New-Item -ItemType Directory -Path $destDir -Force | Out-Null
    }

    $doggy = Join-Path $destDir "doggy.exe"
    Copy-Item $exe $doggy -Force
    Write-Host "[doggy] Installed: $doggy"

    # ACP / IDE often looks for "agent" — same binary, Doggy branding.
    $agent = Join-Path $destDir "agent.exe"
    Copy-Item $exe $agent -Force

    # Never leave a grok.exe process name around.
    foreach ($name in @("grok.exe", "grok.cmd", "grok.exe.old", "grok.exe.doggy-bak")) {
        $p = Join-Path $destDir $name
        if (Test-Path $p) {
            Remove-Item $p -Force
            Write-Host "[doggy] Removed: $p"
        }
    }
}

if (-not $NoInstall) {
    Install-DoggyTo (Join-Path $env:USERPROFILE ".local\bin")
    Install-DoggyTo (Join-Path $env:USERPROFILE ".Doggy\bin")

    # Ensure User PATH has ~/.local/bin and ~/.Doggy/bin (not ~/.grok/bin).
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not $userPath) { $userPath = "" }
    $parts = [System.Collections.Generic.List[string]]::new()
    foreach ($p in ($userPath -split ';' | Where-Object { $_ -and $_.Trim() })) {
        if ($p -match '[\\/]\.grok[\\/]bin\\?$') {
            Write-Host "[doggy] Dropping PATH entry: $p"
            continue
        }
        if (-not $parts.Contains($p)) { $parts.Add($p) }
    }
    foreach ($add in @(
        (Join-Path $env:USERPROFILE ".local\bin"),
        (Join-Path $env:USERPROFILE ".Doggy\bin")
    )) {
        if (-not ($parts | Where-Object { $_ -ieq $add })) {
            $parts.Insert(0, $add)
            Write-Host "[doggy] PATH += $add"
        }
    }
    [Environment]::SetEnvironmentVariable("Path", ($parts -join ';'), "User")
}

# Cursor: show OSC title ("Doggy") on the terminal tab.
$cursorSettings = Join-Path $env:APPDATA "Cursor\User\settings.json"
if (Test-Path (Split-Path $cursorSettings -Parent)) {
    try {
        $raw = if (Test-Path $cursorSettings) { Get-Content $cursorSettings -Raw } else { "{}" }
        if ([string]::IsNullOrWhiteSpace($raw)) { $raw = "{}" }
        $obj = $raw | ConvertFrom-Json
        if ($null -eq $obj) { $obj = [pscustomobject]@{} }
        $obj | Add-Member -NotePropertyName "terminal.integrated.tabs.title" -NotePropertyValue "`${sequence}" -Force
        $obj | ConvertTo-Json -Depth 20 | Set-Content -Path $cursorSettings -Encoding UTF8
        Write-Host "[doggy] Cursor tabs.title = `${sequence}"
    } catch {
        Write-Host "[doggy] WARN: could not patch Cursor settings: $_"
    }
}

if ($Run) {
    Write-Host "[doggy] launching doggy.exe ..."
    $launch = Join-Path $env:USERPROFILE ".local\bin\doggy.exe"
    if (-not (Test-Path $launch)) { $launch = $exe }
    & $launch @args
}
