# Goal criterion worker fan-out proof

This evidence is for **goal criterion workers** (worktree-isolated concurrent holds),
not single-session multi-tool fan-out that writes three files without overlapping holds.

## Worker processes (pid)
- criterion=1 pid=18400
- criterion=2 pid=33700
- criterion=3 pid=38472

## Worktree paths
- criterion=1 worktree=C:\Users\Tylergao\AppData\Local\Temp\fanout-live-wt\w1
- criterion=2 worktree=C:\Users\Tylergao\AppData\Local\Temp\fanout-live-wt\w2
- criterion=3 worktree=C:\Users\Tylergao\AppData\Local\Temp\fanout-live-wt\w3

## Concurrency
- wall-clock triple overlap: max_start_ms=1785832305602 < min_end_ms=1785832314053 (overlap_ms=8451)
- each hold_ms >= 8000 with real sleep in a separate process
- three distinct pids and three distinct git worktree paths
