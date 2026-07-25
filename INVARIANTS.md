# INVARIANTS — what must stay true

> Part of this project's agent-model (see `CLAUDE.md` → "Agent-Model & the prototype-driven loop").
> Structured "what must be true" register — guardrails, not implementation. *What* must
> hold, never *how* it's done (that's the agent's domain).
>
> Each invariant is tagged:
> - **ENFORCED** — a test / check / CI gate proves it. Cannot silently rot.
> - **PROSE** — relied on but unguarded. A candidate to *graduate* into an enforced check.
>
> The `/close` loop's standing job is to move invariants PROSE → ENFORCED. A rising
> enforced ratio is the signal the model is earning its keep.
>
> **Authority precedence** (see `PROTOTYPE-DRIVEN.md §1a` for the full rationale —
> the prototype is the oracle for *how it behaves*, not for *what must be true*):
>   1. Legal / security / privacy obligations
>   2. ENFORCED invariants (this file, tagged ENFORCED)
>   3. Reviewer consensus on risky changes
>   4. Running-prototype behavior
>   5. Recalled lessons / decisions
>   6. Generated / derived views (PROSE invariants sit on this rung)
>
> **Reconciled only against code and lived experience — never against itself.** If an
> entry here contradicts the running product on a rung-4 question, the product wins
> and this file is wrong.

## Release / propagation

<!-- prompt: what version-bearing files must stay in sync? what's the ship gate? -->

## Contracts (the cross-surface edges)

<!-- prompt: what API/schema shapes ripple across surfaces? what's the envelope contract? -->

## Runtime

<!-- prompt: what concurrency / process / connection / resource invariants must hold? -->

## Secrets / configuration

<!-- prompt: which secrets live where? what's the boundary nobody crosses? -->

## CI / dev workflow

<!-- prompt: what must pass before a change can merge? -->

## Open tensions (NOT invariants — tracked drift to resolve)

<!-- prompt: what's currently broken or inconsistent and needs to converge? -->

<!-- Add project-specific sections below (e.g. "Spaces / governance", "Extraction / render",
     "Observability / privacy", "Hosted / multi-user"). See knowmarks/INVARIANTS.md for the
     proven shape of a populated file. -->
