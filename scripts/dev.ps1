# Incremental dev loop: rebuild changed crates and run the TUI.
# After the first successful build, this is usually seconds–minutes, not a full rebuild.
#
# Usage (from repo root or any cwd):
#   .\scripts\dev.ps1
#   .\scripts\dev.ps1 -- --help

$ErrorActionPreference = "Stop"
# $PSScriptRoot, not the cwd: this is dot-sourced before Set-Location below.
. (Join-Path $PSScriptRoot "cargo.ps1")
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $Root

$protoc = Join-Path $Root ".tools\protoc\bin\protoc.exe"
if (Test-Path $protoc) {
    $env:PROTOC = $protoc
}

Write-Host "[doggy] cargo run -p xai-grok-pager-bin (incremental) ..."
# Propagated rather than thrown: this is a launcher, so the exit code that
# matters is the TUI's own, and a compile failure already surfaces as non-zero.
Invoke-Cargo run -p xai-grok-pager-bin -- @args
exit $LASTEXITCODE
