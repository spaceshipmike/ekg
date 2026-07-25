# Model decisions

> The decisions that constrain how this project is built. Each entry:
> **decision**, **why**, **revisit trigger**. Companion to `INVARIANTS.md`
> (what must be true) and `SURFACES.md` (where changes ripple).

---

## D-001 · Hand-rolled JSON over serde

**Decision.** The `--log` recorder serializes JSONL by hand (`src/recorder.rs::escape_json`) instead of adding serde.

**Why.** The README promises "fast and tiny — four dependencies"; the recorder writes three fixed shapes with one string field. A serialization framework buys nothing here and costs compile time, binary size, and the marketing claim. (Issue #3 / PR #10.)

**Revisit trigger.** A third structured-output surface (e.g. a `--json` status output or config file) that makes hand-escaping error-prone.

---

## D-002 · Hook timeout enforced inside the hook's own process tree

**Decision.** The 30s hook timeout is a watchdog *inside* the spawned shell wrapper (group leader via `process_group(0)`, self-group-kill), not a tokio task inside ekg. ekg's side only reaps.

**Why.** An in-ekg watchdog dies with ekg — Ctrl-C or a `--count` exit would leave a hung hook running forever, contradicting the documented bound. Codex proved the escape paths concretely over rounds 3–5 of PR #11 (surviving descendants, TERM-immune children, watchdog death). Self-termination makes the guarantee hold by construction.

**Revisit trigger.** A platform where `process_group(0)`/group-kill semantics don't hold (Windows port), or a need for per-hook configurable timeouts.

---

## D-003 · Single-flight hooks (skip, don't queue or kill)

**Decision.** Per target and per hook kind, a new event while the previous hook still runs is *skipped* — not queued, not killing the predecessor.

**Why.** Hooks exist for notifications and automations (power-cycle a router); cutting one off mid-run is worse than dropping a duplicate signal, and a queue turns a flapping link into a notification storm — the opposite of what a monitor should do. (PR #11.)

**Revisit trigger.** A real user need for guaranteed per-event delivery (at that point: an event queue with dedup, not unbounded spawn).

---

## D-004 · POSIX-sh-lowest-common-denominator for shipped shell

**Decision.** Any shell ekg ships (the hook wrapper) targets the POSIX subset that dash accepts, verified by dash-exercising tests — even where bash is more expressive.

**Why.** `/bin/sh` is dash on Debian/Ubuntu — the majority of ekg's Linux audience. PR #11 shipped a group-kill that worked in bash and silently no-opped in dash (`kill -- -pgid` parse failure), which would have voided the timeout guarantee in production. macOS dev machines cannot catch this without deliberately testing under dash.

**Revisit trigger.** Dropping the shell wrapper entirely (e.g. a native process-tree kill), which would retire the constraint.

---

## Provenance

D-001 from issue #3 / PR #10 (2026-07-25). D-002–D-004 from issue #4 / PR #11's six Codex review rounds plus the Ubuntu CI failure investigation (2026-07-25) — each encodes a defect that was actually caught, not a hypothetical. Companion lessons in `.fctry/lessons.md`.
