# Shared cargo invocation for the scripts in this folder.
#
# cargo writes *all* of its progress to stderr, and `$ErrorActionPreference =
# "Stop"` turns any native command's stderr write into a terminating
# NativeCommandError the moment stderr is redirected. In an interactive console
# stderr goes straight to the terminal and nothing is redirected, so the build
# scripts worked when a human ran them and failed on the first "Compiling ..."
# line under an agent, CI, or a plain `> build.log 2>&1`. That looks like a
# broken build rather than a broken script, which is what made it expensive.
#
# The exit code is the honest signal for whether cargo succeeded, so callers
# read $LASTEXITCODE after this returns.

# Deliberately no param block. A [Parameter(ValueFromRemainingArguments)]
# array looks like the right way to forward arguments and is not: PowerShell
# still parses `-p` as a parameter name and silently drops it *and* its value,
# so `cargo build -p xai-grok-pager-bin --release` degrades to a release build
# of the whole workspace with no error anywhere. The automatic $args of a
# param-less function keeps every token verbatim.
function Invoke-Cargo {
    # Function-scoped by PowerShell's copy-on-write scoping: the caller keeps
    # its own "Stop" for everything that is not this cargo call.
    $ErrorActionPreference = "Continue"

    & cargo @args
}

# Run cargo and stop the script if it failed.
function Invoke-CargoOrThrow {
    Invoke-Cargo @args
    if ($LASTEXITCODE -ne 0) {
        throw "cargo $($args -join ' ') failed with exit code $LASTEXITCODE"
    }
}
