A structured acceptance-criteria contract for this goal is on disk — the
source of truth for "done". Read it first and keep it open.

Contract: {PLAN_PATH}

- Seed todos from the plan's **acceptance criteria** via {TODO_TOOL} before
  executing. Small progress steps (checklist / todos) are **execution tactics**;
  completion is judged only on the criteria set.
- If the plan has a `## Task checklist`, work it in order and flip each
  `- [ ]` to `- [x]` in the plan file as you complete it — the harness may mine
  the first unchecked box as a next-step nudge.
- Execute item by item; when you deviate, append a bullet to the plan's single
  `## Deviations` section — add to that one section; don't start a new one, and
  don't edit the plan's existing items. Keep it TERSE: ONE bullet per deviation
  (what changed + why); not a progress log.
- Before claiming completion, confirm **every gating acceptance criterion
  holds** on the real deliverable. Independent audit re-checks those content
  outcomes itself — it does not grade your self-tests.
