# Plan: Live goal multi-worker fan-out with wall-clock overlap proof

## Goal kind
code-change

## Objective
Prove **goal-layer** criterion-worker fan-out actually ran: ≥2 concurrent workers in separate git worktrees, with **overlapping wall-clock intervals** recorded in workspace artifacts. This is **not** same-turn multi-tool writes by a single implementer.

## Why this contract forces fan-out
- Product path: `maybe_run_criterion_wave` → `plan_wave` → `run_wave` (`join_all`) → one subagent per criterion with `SubagentIsolationMode::Worktree` → ordered `merge_wave` → parent sets Exec.
- Preconditions (already true on this machine unless overridden):
  - `[goal] fanout_max = 3` (or `GROK_GOAL_FANOUT_MAX≥2`)
  - Workspace is a git repo
  - Worktrees are **not** disposed before merge
- Contract shape: criteria 1–3 are **independent**, **disjoint write scopes**, ≥2 ready at once → eligible for one wave.
- Proof is **in-repo timestamps + PID + worktree path**, not narrative claims.

## Acceptance criteria
1. Workspace path `fanout_live/w1/meta.json` exists as UTF-8 JSON with at least these keys and types:
   - `criterion` = number `1`
   - `started_unix_ms` = integer (Unix epoch ms when this worker began its proof work)
   - `ended_unix_ms` = integer (Unix epoch ms when this worker finished; must be `> started_unix_ms`)
   - `pid` = integer (process id of the shell/process that slept)
   - `worktree` = non-empty string (absolute path of the git worktree cwd used for this write; must **not** equal the main repo root `D:\Doggy` if a worktree was allocated — if the runtime only exposes the main root, still record actual cwd as absolute path)
   - `hold_ms` = integer ≥ `8000` (this worker must hold/sleep at least 8 seconds between start and end stamps)
2. Workspace path `fanout_live/w2/meta.json` — same schema as (1) but `criterion` = `2`, under write scope `fanout_live/w2/` only.
3. Workspace path `fanout_live/w3/meta.json` — same schema as (1) but `criterion` = `3`, under write scope `fanout_live/w3/` only.
4. Workspace path `fanout_live/OVERLAP.md` exists and contains **all** of:
   - the three strings `criterion=1`, `criterion=2`, `criterion=3`
   - a line exactly: `overlap: YES`
   - a line matching: `max_start_ms=<digits>`
   - a line matching: `min_end_ms=<digits>`
   - with the numeric property **max(started_unix_ms of w1,w2,w3) < min(ended_unix_ms of w1,w2,w3)** (true wall-clock overlap of all three intervals)
5. Workspace path `fanout_live/PROOF.md` exists and states in plain text that evidence is for **goal criterion workers** (not single-session multi-tool fan-out), and includes the three `pid` values and three `worktree` paths copied from the meta files.

## Acceptance checklist
| Exec | Audit | Criterion |
|------|-------|-----------|
| [ ] | [ ] | `fanout_live/w1/meta.json` valid with criterion=1, timestamps, pid, worktree, hold_ms≥8000 |
| [ ] | [ ] | `fanout_live/w2/meta.json` valid with criterion=2, timestamps, pid, worktree, hold_ms≥8000 |
| [ ] | [ ] | `fanout_live/w3/meta.json` valid with criterion=3, timestamps, pid, worktree, hold_ms≥8000 |
| [ ] | [ ] | `fanout_live/OVERLAP.md` with overlap: YES and max_start_ms < min_end_ms across all three |
| [ ] | [ ] | `fanout_live/PROOF.md` documents pids + worktrees and goal-worker (not multi-tool) intent |

## Criterion dependencies
| # | Depends on | Write scope |
|---|------------|-------------|
| 1 | - | fanout_live/w1/ |
| 2 | - | fanout_live/w2/ |
| 3 | - | fanout_live/w3/ |
| 4 | 1, 2, 3 | fanout_live/OVERLAP.md |
| 5 | 1, 2, 3, 4 | fanout_live/PROOF.md |

## Waves (expected)
| Wave | Criteria | Notes |
|------|----------|-------|
| 1 | 1, 2, 3 | Must fan out (3 ready, disjoint scopes, fanout_max≥3). Each worker holds ≥8s → serial wall ≥24s; parallel wall ≈8–12s with overlap. |
| 2 | 4 | Join: compute overlap from the three meta.json files only. |
| 3 | 5 | Human-readable proof doc. |

## Deterministic checks
| # | Criterion | Command |
|---|-----------|---------|
| 1 | 1 | powershell -NoProfile -Command " $j = Get-Content -Raw 'fanout_live/w1/meta.json' \| ConvertFrom-Json; if ($j.criterion -eq 1 -and $j.ended_unix_ms -gt $j.started_unix_ms -and $j.hold_ms -ge 8000 -and $j.pid -gt 0 -and $j.worktree) { exit 0 } else { exit 1 } " |
| 2 | 2 | powershell -NoProfile -Command " $j = Get-Content -Raw 'fanout_live/w2/meta.json' \| ConvertFrom-Json; if ($j.criterion -eq 2 -and $j.ended_unix_ms -gt $j.started_unix_ms -and $j.hold_ms -ge 8000 -and $j.pid -gt 0 -and $j.worktree) { exit 0 } else { exit 1 } " |
| 3 | 3 | powershell -NoProfile -Command " $j = Get-Content -Raw 'fanout_live/w3/meta.json' \| ConvertFrom-Json; if ($j.criterion -eq 3 -and $j.ended_unix_ms -gt $j.started_unix_ms -and $j.hold_ms -ge 8000 -and $j.pid -gt 0 -and $j.worktree) { exit 0 } else { exit 1 } " |
| 4 | 4 | powershell -NoProfile -Command " $a=(Get-Content -Raw 'fanout_live/w1/meta.json'\|ConvertFrom-Json); $b=(Get-Content -Raw 'fanout_live/w2/meta.json'\|ConvertFrom-Json); $c=(Get-Content -Raw 'fanout_live/w3/meta.json'\|ConvertFrom-Json); $ms=[Math]::Max([Math]::Max($a.started_unix_ms,$b.started_unix_ms),$c.started_unix_ms); $me=[Math]::Min([Math]::Min($a.ended_unix_ms,$b.ended_unix_ms),$c.ended_unix_ms); $t=Get-Content -Raw 'fanout_live/OVERLAP.md'; if ($ms -lt $me -and $t -match 'overlap: YES' -and $t -match 'criterion=1' -and $t -match 'criterion=2' -and $t -match 'criterion=3') { exit 0 } else { exit 1 } " |
| 5 | 5 | powershell -NoProfile -Command " $t=Get-Content -Raw 'fanout_live/PROOF.md'; if ($t -match 'goal criterion' -and $t -match 'pid' -and $t -match 'worktree') { exit 0 } else { exit 1 } " |

> **Shell note:** Prefer no `$var` / backtick escapes in checks if the harness re-expands through PowerShell. If a check fails with parse errors, rewrite using `ConvertFrom-Json` pipelines without intermediate `$` where possible, or run via a small `.ps1` under `fanout_live/` committed only if needed. Content outcomes remain the source of truth for audit.

## Verification plan
1. gating: Read `fanout_live/w1/meta.json` — schema + hold_ms≥8000.
2. gating: Read `fanout_live/w2/meta.json` — same for criterion 2.
3. gating: Read `fanout_live/w3/meta.json` — same for criterion 3.
4. gating: Compute overlap: `max(starts) < min(ends)`; read `OVERLAP.md` for `overlap: YES`.
5. gating: Read `PROOF.md` for pids/worktrees and goal-worker wording.
6. evidence (soft): Session events `goal_fanout_started` with criteria `[1,2,3]` and `goal_fanout_finished` with `landed` including 1–3 — if available in UI/logs; **not required to pass audit** if workspace proof holds.
7. anti-cheat note: A single process writing three files without true concurrency will struggle to get three overlapping ≥8s holds unless it fakes timestamps. Auditors should treat `ended - started ≥ 8000` per file **and** triple overlap as the bar. PIDs may coincide if workers share a host process model; **overlap of holds** is the primary concurrency signal; distinct worktree paths are secondary evidence when present.

## Implementation approach (for criterion workers)
### Wave 1 — each of criteria 1–3 (one worker each; do not touch siblings' dirs)
```powershell
# Example for criterion N in 1..3 — run only inside fanout_live/wN/
$dir = "fanout_live/wN"
New-Item -ItemType Directory -Force -Path $dir | Out-Null
$start = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$hold = 8000
Start-Sleep -Milliseconds $hold
$end = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
$meta = [ordered]@{
  criterion = N
  started_unix_ms = $start
  ended_unix_ms = $end
  hold_ms = ($end - $start)
  pid = $PID
  worktree = (Get-Location).Path
}
$meta | ConvertTo-Json | Set-Content -Path "$dir/meta.json" -Encoding utf8
```
- Sleep **≥8000 ms** is mandatory so wall-clock overlap is meaningful.
- Write **only** under your `write_scope`.
- Do **not** edit `plan.md` or sibling `w*` directories.
- Do **not** claim Exec (parent sets Exec after merge).

### Wave 2 — criterion 4 (after 1–3 Exec claimed / merged)
- Read the three `meta.json` files from the main tree (post-merge).
- Compute `max_start_ms` and `min_end_ms`.
- If `max_start_ms < min_end_ms`, write `overlap: YES`; else `overlap: NO` (fail criterion).

### Wave 3 — criterion 5
- Write `PROOF.md` summarizing pids, worktrees, overlap numbers, and that this was goal criterion-worker fan-out.

## Coordinating session rules
- **Do not** implement criteria 1–3 yourself in one session if the orchestrator can fan out — let `maybe_run_criterion_wave` spawn workers.
- If fan-out declines (`disabled` / `no_repo` / `worktrees_disposed` / `not_enough_ready`), record the reason in `fanout_live/PROOF.md` and still attempt serial completion, but **audit of criterion 4 must fail** without true interval overlap (serial 8s+8s+8s cannot triple-overlap).
- After workers land, only then do criteria 4–5.

## Non-goals
- No product crate / config / `fanout_max` changes (use existing `fanout_max = 3`).
- No changes outside `fanout_live/`.
- No requirement to modify `parallel_verify/` or `parallel_exp/`.
- No unit-test-only pass: workspace artifacts required.
- Not satisfied by same-turn multi-`write` without ≥8s overlapping holds.

## Assumed scope
- New directory only: `fanout_live/` under workspace root.
- Files: `w1/meta.json`, `w2/meta.json`, `w3/meta.json`, `OVERLAP.md`, `PROOF.md`.

## Task checklist
- [ ] Confirm `fanout_max ≥ 2` and git repo available
- [ ] Wave 1: workers produce w1/w2/w3 meta.json with ≥8s holds
- [ ] Observe or infer fan-out (events optional; overlap required)
- [ ] Wave 2: OVERLAP.md with overlap: YES
- [ ] Wave 3: PROOF.md with pids + worktrees
- [ ] All Exec ticked; request completion for Audit

## Success definition
**Achieved** only if all five acceptance criteria hold on disk **and** the three time intervals truly overlap. That is the live proof that work was concurrent at wall-clock level under a multi-criterion goal wave — the bar the previous `parallel_verify` goal explicitly did not require.
