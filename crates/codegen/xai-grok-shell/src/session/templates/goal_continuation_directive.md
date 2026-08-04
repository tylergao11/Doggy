<system-reminder>
<goal-state>
Objective: {objective}
Status: Active
Tokens: {tokens} | Elapsed: {elapsed}
</goal-state>

{bail_preface}{plan_pointer}{verifier_gaps}{strategist_note}{reverify_block}{ready_wave}Goal NOT complete — continue working. Next step:
{next_step}

Keep your {todo_tool} list current (≥1 `in_progress`, descriptive
`activeForm`). Work the dual **Acceptance checklist** (Exec column first).
Scratch dir {scratch_dir} {scratch_status} is only for throwaway notes/artifacts
(never shared `/tmp/...`). Use existing user/system defaults for dependencies;
do not point package-manager homes or config at scratch.

Independent audit judges **each acceptance criterion** against the shipped
content. Make those outcomes true and observable on the deliverable.

Before `{goal_tool}(completed: true)`, every **Exec** cell on the Acceptance
checklist must be `[x]`. The harness rejects incomplete Exec and updates
**Audit** marks after verification.
</system-reminder>
