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
# $PSScriptRoot, not the cwd: this is dot-sourced before Set-Location below.
. (Join-Path $PSScriptRoot "cargo.ps1")
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $Root

$protoc = Join-Path $Root ".tools\protoc\bin\protoc.exe"
if (Test-Path $protoc) {
    $env:PROTOC = $protoc
}

$pkg = "xai-grok-pager-bin"
if ($Debug) {
    Write-Host "[doggy] cargo build -p $pkg (debug) ..."
    Invoke-CargoOrThrow build -p $pkg
    $exe = Join-Path $Root "target\debug\xai-grok-pager.exe"
} else {
    Write-Host "[doggy] cargo build -p $pkg --release ..."
    Invoke-CargoOrThrow build -p $pkg --release
    $exe = Join-Path $Root "target\release\xai-grok-pager.exe"
}

if (-not (Test-Path $exe)) {
    throw "Build finished but binary missing: $exe"
}

$sizeMb = [math]::Round((Get-Item $exe).Length / 1MB, 1)
Write-Host "[doggy] OK: $exe ($sizeMb MB)"

function Copy-DoggySafe([string]$src, [string]$destPath) {
    # Never kill a running doggy — that aborts the Agent that invoked us.
    #
    # On Windows a *running* .exe can usually be RENAMED even when it cannot
    # be overwritten. New launches then pick up the fresh path. Falling back
    # only to doggy-next.exe left `where doggy` permanently resolving to the
    # locked old doggy.exe — the "I open a new window and it's still old"
    # failure mode.
    $dir = Split-Path $destPath -Parent
    $base = [System.IO.Path]::GetFileNameWithoutExtension($destPath)
    $ext = [System.IO.Path]::GetExtension($destPath)
    $prev = Join-Path $dir ($base + "-prev" + $ext)
    $next = Join-Path $dir ($base + "-next" + $ext)

    # Fast path: direct overwrite.
    try {
        Copy-Item $src $destPath -Force -ErrorAction Stop
        return $destPath
    } catch {
        Write-Host "[doggy] direct copy locked: $destPath — trying rename-then-replace"
    }

    # Rename the locked image out of the way, then install as doggy.exe.
    try {
        if (Test-Path $prev) {
            Remove-Item $prev -Force -ErrorAction SilentlyContinue
        }
        if (Test-Path $destPath) {
            Rename-Item -LiteralPath $destPath -NewName ([System.IO.Path]::GetFileName($prev)) -Force -ErrorAction Stop
        }
        Copy-Item $src $destPath -Force -ErrorAction Stop
        Write-Host "[doggy] Installed via rename-replace: $destPath"
        Write-Host "[doggy]   (running session keeps the old image as $prev until exit)"
        Write-Host "[doggy]   NEW shells typing 'doggy' will get this binary."
        return $destPath
    } catch {
        Write-Host "[doggy] rename-replace failed: $_"
    }

    # Last resort: side-by-side name (does NOT fix `where doggy`).
    Copy-Item $src $next -Force -ErrorAction Stop
    Write-Host "[doggy] WARN: could not replace $destPath. Wrote: $next"
    Write-Host "[doggy]        Launch THAT path explicitly — 'doggy' may still be the old file."
    Write-Host "[doggy]        Do NOT kill the current doggy process from Agent."
    return $next
}

function Install-DoggyTo([string]$destDir) {
    if (-not (Test-Path $destDir)) {
        New-Item -ItemType Directory -Path $destDir -Force | Out-Null
    }

    $doggy = Join-Path $destDir "doggy.exe"
    $installed = Copy-DoggySafe $exe $doggy
    Write-Host "[doggy] Installed: $installed"

    # ACP / IDE often looks for "agent" — same binary, Doggy branding.
    $agent = Join-Path $destDir "agent.exe"
    $agentInstalled = Copy-DoggySafe $exe $agent
    if ($agentInstalled -ne $agent) {
        Write-Host "[doggy] agent side-install: $agentInstalled"
    }

    # Never leave a grok.exe process name around.
    foreach ($name in @("grok.exe", "grok.cmd", "grok.exe.old", "grok.exe.doggy-bak")) {
        $p = Join-Path $destDir $name
        if (Test-Path $p) {
            try {
                Remove-Item $p -Force -ErrorAction Stop
                Write-Host "[doggy] Removed: $p"
            } catch {
                Write-Host "[doggy] WARN: could not remove $p (in use): $_"
            }
        }
    }
}

if (-not $NoInstall) {
    # Install to BOTH locations. Users historically had ~/.Doggy/bin first on
    # PATH while only ~/.local/bin got updates → old deadlock binary kept
    # winning (Checking forever, cancel.processing never logged).
    Install-DoggyTo (Join-Path $env:USERPROFILE ".local\bin")
    Install-DoggyTo (Join-Path $env:USERPROFILE ".Doggy\bin")

    # Ensure User PATH has ~/.Doggy/bin and ~/.local/bin (not ~/.grok/bin),
    # and force-reorder so both stay near the front even if they already
    # existed later in PATH (Insert(0) alone is a no-op when present).
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (-not $userPath) { $userPath = "" }
    $localBin = Join-Path $env:USERPROFILE ".local\bin"
    $doggyBin = Join-Path $env:USERPROFILE ".Doggy\bin"
    $parts = [System.Collections.Generic.List[string]]::new()
    foreach ($p in ($userPath -split ';' | Where-Object { $_ -and $_.Trim() })) {
        if ($p -match '[\\/]\.grok[\\/]bin\\?$') {
            Write-Host "[doggy] Dropping PATH entry: $p"
            continue
        }
        # Strip so we can re-insert at front in a fixed order.
        if ($p -ieq $localBin -or $p -ieq $doggyBin) { continue }
        if (-not $parts.Contains($p)) { $parts.Add($p) }
    }
    # Prefer ~/.local/bin first: install always succeeds there even when
    # ~/.Doggy/bin/doggy.exe is locked by a running session (which then only
    # gets doggy-next.exe). Putting .Doggy first caused weeks of "fixed in
    # source but still broken" — where doggy kept resolving to the old lock.
    $parts.Insert(0, $doggyBin)
    $parts.Insert(0, $localBin)
    Write-Host "[doggy] PATH front: $localBin ; $doggyBin"
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
