# AGENTS.md

Runtime-neutral project instructions for coding agents.

## Agent-Model & the prototype-driven loop

<!-- inserted by fctry init-agent-model -->

This project follows fctry's prototype-driven methodology. The running product
is the source of truth for behavior; the files below preserve the constraints
and learning that must travel across sessions and runtimes.

The agent-model files:

- **`INVARIANTS.md`** — what must stay true (ENFORCED vs PROSE)
- **`SURFACES.md`** — the cross-surface ripple map
- **`MODEL_DECISIONS.md`** — decisions, reasons, and revisit triggers
- **`.fctry/lessons.md`** — running learnings

Forward-looking design lives in `designs/<slug>.md`, using
`designs/_template.md`. A design note is temporary: distill accepted decisions
into the agent model and issues, then archive it under `designs/_archived/`.

The loop is: *experience → issue → Diagnose → Scope → Build in a task-owned
worktree → local checks → CI → independent review → `/close`*. Final merging
remains operator-authorized.

Use the runtime-neutral core for deterministic behavior:

```text
fctry host capabilities --json
fctry project mode --json
fctry close inspect --json
fctry phase validate <packet.json> --json
fctry task create|claim|renew|complete|review
fctry message send|ack|reject
fctry recover --json
```

Shared primitives are resolved from `FCTRY_PLUGIN_ROOT`. Runtime adapters may
populate compatibility aliases, but project instructions and custom scripts
must not require a Claude- or Codex-specific plugin-root variable.

Session state is namespaced under `.fctry/sessions/`. Do not hand-edit shared
state or another session's record. Write-capable tasks declare scope and use a
task-owned worktree and branch; overlapping active scopes are rejected.

For substantive user-facing reports, apply the shared `operator-comms` skill:
plain language must preserve the outcome, material detail, practical
consequences, uncertainty, recommendation, and next decision.

Sentinels:

- `.fctry/no-session-handoff` — use the host's canonical handoff only
- `.fctry/substrate-tier` — substrate sizing
- `.fctry/agent-model-init` — agent-model bootstrap date
