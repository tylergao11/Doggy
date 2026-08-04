"""Batch-fix remaining Windows / Doggy test shape failures."""
from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(r"D:\Doggy\crates\codegen\xai-grok-pager\src")


def add_cfg_not_windows_to_tests(path: Path, names: list[str]):
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines(keepends=True)
    fn_re = re.compile(r"^(\s*)fn\s+(\w+)\s*\(")
    idx = {}
    for i, line in enumerate(lines):
        m = fn_re.match(line)
        if m and m.group(2) in names:
            idx[m.group(2)] = (i, m.group(1))
    missing = [n for n in names if n not in idx]
    if missing:
        raise SystemExit(f"missing fns in {path.name}: {missing}")

    for name in sorted(idx, key=lambda n: -idx[n][0]):
        i, indent = idx[name]
        # find start of attr/doc block
        start = i
        k = i - 1
        already = False
        while k >= 0:
            s = lines[k].strip()
            if "cfg(not(windows))" in s or 'cfg(not(target_os = "windows"))' in s:
                already = True
                break
            if s.startswith("#[") or s.startswith("///") or s.startswith("//!"):
                start = k
                k -= 1
                continue
            break
        if already:
            print(f"  already cfg: {name}")
            continue
        lines.insert(start, f"{indent}#[cfg(not(windows))]\n")
        print(f"  cfg(not(windows)): {name}")

    path.write_text("".join(lines), encoding="utf-8", newline="\n")
    print(f"ok: {path.name}")


# --- shell completion Tab tests (Unix-only product surface) ---
add_cfg_not_windows_to_tests(
    ROOT / "app/agent_view/shell_completion.rs",
    [
        "esc_closes_tab_fetched_dropdown",
        "prompt_click_invalidates_cached_items_before_tab",
        "repeat_tab_fires_single_fetch_while_pending",
        "tab_fill_clipping_paste_chip_opens_dropdown_without_refetch",
        "tab_fill_kicks_deterministic_fetch_always_on",
        "tab_fills_common_prefix_then_opens_dropdown_on_refresh",
        "tab_insta_accept_clipping_paste_chip_opens_dropdown_without_refetch",
        "tab_mixed_file_and_history_items_opens_dropdown",
        "tab_mixed_rangeless_and_ranged_rows_open_dropdown",
        "tab_opens_dropdown_without_ghost",
        "tab_single_candidate_accepts_and_kicks_fetch_always_on",
        "tab_single_history_item_opens_dropdown",
        "tab_single_token_candidate_accepts_without_dropdown_flash",
        "tab_sole_rangeless_path_row_opens_dropdown_never_accepts",
        "tab_whole_line_history_items_open_dropdown_not_fill",
        "tab_with_stale_items_refetches",
        "tab_without_items_fires_deterministic_fetch",
    ],
)

add_cfg_not_windows_to_tests(
    ROOT / "app/dispatch/tests/prompt.rs",
    [
        "tab_fetch_landing_insta_accepts_single_candidate_always_on",
        "tab_fetch_landing_opens_dropdown_for_ambiguous_set_always_on",
    ],
)

# --- hooks: unix-only (spawns `sh`) ---
HOOKS = ROOT / "notifications/hooks.rs"
ht = HOOKS.read_text(encoding="utf-8")
ht2, n = re.subn(
    r"#\[cfg\(test\)\]\s*\nmod tests \{",
    "#[cfg(all(test, unix))]\nmod tests {",
    ht,
    count=1,
)
if n:
    HOOKS.write_text(ht2, encoding="utf-8", newline="\n")
    print("ok: hooks tests unix-only")
else:
    print("skip/warn: hooks tests mod not rewritten", "all(test, unix)" in ht)

# --- billing Doggy rebrand ---
BILLING = ROOT / "app/dispatch/tests/billing.rs"
bt = BILLING.read_text(encoding="utf-8")
bt2 = bt.replace("Grok Build", "Doggy")
if bt2 != bt:
    BILLING.write_text(bt2, encoding="utf-8", newline="\n")
    print("ok: billing Grok Build -> Doggy")
else:
    print("skip: billing already Doggy")

print("batch phase1 done")
