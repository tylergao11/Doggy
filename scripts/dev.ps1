# Incremental dev loop: rebuild changed crates and run the TUI.
# After the first successful build, this is usually seconds–minutes, not a full rebuild.
#
# Usage (from repo root or any cwd):
#   .\scripts\dev.ps1
#   .\scripts\dev.ps1 -- --help

$ErrorActionPreference = "Stop"
$Root = Split-Path -Parent (Split-Path -Parent $MyInvocation.MyCommand.Path)
Set-Location $Root

$protoc = Join-Path $Root ".tools\protoc\bin\protoc.exe"
if (Test-Path $protoc) {
    $env:PROTOC = $protoc
}

Write-Host "[doggy] cargo run -p xai-grok-pager-bin (incremental) ..."
cargo run -p xai-grok-pager-bin -- @args
