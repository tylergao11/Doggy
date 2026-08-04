# Parallel capability experiment — facts

Date: 2026-08-04  
Workspace: `D:\Doggy`  
Scope: unit-test verification of `session::goal_fanout` and `session::goal_criterion_graph` (no product source changes; no live multi-worker fan-out).

## Marker files

`parallel_exp/fanout_ok.txt`:

```
goal_fanout: PASS
```

`parallel_exp/graph_ok.txt`:

```
goal_criterion_graph: PASS
```

## Test runs (this experiment)

### 1. `cargo test -p xai-grok-shell --lib session::goal_fanout::`

```
test result: ok. 23 passed; 0 failed; 0 ignored; 0 measured; 5408 filtered out; finished in 0.76s
```

Coverage exercised includes concurrent wave, disjoint landing, merge order, and decline paths (e.g. `a_wave_runs_its_workers_concurrently`, `disjoint_workers_both_land_in_the_repo`, `a_wave_merges_in_criterion_order`, `fanout_is_off_unless_configured`, `fanout_declines_when_the_work_could_not_be_landed`).

### 2. `cargo test -p xai-grok-shell --lib session::goal_criterion_graph::`

```
test result: ok. 27 passed; 0 failed; 0 ignored; 0 measured; 5404 filtered out; finished in 0.00s
```

Coverage exercised includes parallel waves, scope conflict serialization, and serial fallback (e.g. `independent_criteria_share_a_wave`, `serialize_conflicts_collapses_an_all_undeclared_contract_to_serial`, `load_falls_back_to_serial_for_absent_and_broken_contracts`, `overlapping_scope_without_an_edge_is_a_conflict`).

## Live fan-out default (fact)

Live criterion-worker fan-out stays **off** unless `[goal] fanout_max > 1` (config) or `GROK_GOAL_FANOUT_MAX` (env) raises the cap. Product default is serial:

- `GOAL_FANOUT_MAX_DEFAULT = 1` in `session::goal_fanout`
- Resolved via `resolve_goal_fanout_max()` (`GROK_GOAL_FANOUT_MAX` → config `fanout_max` → default `1`)

This experiment therefore verifies the **shipped parallel logic via unit tests** and freezes the pass markers above; it does not claim this session spawned multi-worker criterion fan-out under default config.
