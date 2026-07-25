# Design note: <slug — short, hyphenated, matches filename>

> **Status:** Draft — design note, awaiting decision. **Non-canonical scaffolding**
> (prototype-driven): this exists to win a direction and then *distill into*
> `MODEL_DECISIONS.md` D-entries + any new `INVARIANTS.md` lines + any new
> `SURFACES.md` rows + a set of build issues. The running prototype stays the
> oracle. Once the decisions land and the issues file, **archive this** to
> `designs/_archived/`.
>
> **Date:** YYYY-MM-DD · **Author:** drafted with Claude (or operator name)
>
> **Grounding:** [`<slug>-research.md`](./<slug>-research.md) (sibling research note, if any).
> Also cite relevant cycle-scoped research verdicts: `.fctry/refs/<UTC>-<slug>.json`.
> The Grounding field carries the design's prior-art trail — research notes and verdicts
> are CITED, not absorbed, so provenance stays at its source.
>
> **Decisions this would produce:** D-0NN *<one-line decision>*; D-0NN *<one-line decision>*.
> Each D-0NN is a placeholder for a future `MODEL_DECISIONS.md` entry that
> distills the design's conclusion (decision / why / revisit trigger).
>
> **Invariants it would touch:** cite existing entries by slug ("L96
> `orchestrator-no-product-edits` is strengthened, not relaxed"); flag any
> NEW invariants the design would add as `[NEW]` candidates.
>
> **Surfaces it would touch:** cite existing `SURFACES.md` rows ("the row for
> `src/scheduled/`"); flag any NEW rows the design would add as `[NEW]`
> candidates.

---

## The decision in one line

<TODO: a single sentence that captures the design's bottom line. If you can't
write this sentence, the design isn't ready to write yet.>

## Why now — what motivates the change

<TODO: the forcing function. What broke, what shifted, what gap surfaced. Cite
the specific failure or insight that made this worth the design effort.>

## The architecture / design

<TODO: the design itself. Compose-with-existing where possible — name the
existing primitives the design builds on. New primitives get their own
sub-section with motivation + shape.>

### Composes with existing

<TODO: enumerate existing primitives the design relies on.>

### New primitives (if any)

<TODO: each new primitive gets a sub-section.>

## Risks + open questions

<TODO: what could go wrong; what's still undecided. Each open question is a
specific decision the loop's Diagnose phase will need to settle.>

## Phasing — as issues

Each phase = a loop unit (a build-able issue with a clean diagnose target).
Numbered for ordering; the loop ships them one cycle at a time.

1. **Phase 1: <name>** — <what ships; effort estimate; dependencies>
2. **Phase 2: <name>** — <what ships; effort estimate; dependencies>
3. **Phase 3: <name>** — <what ships; effort estimate; dependencies>

## How this lands in the agent-model

When the design's direction is approved, the distill targets are:

- **`MODEL_DECISIONS.md`** D-entries: <list each D-0NN with its one-line decision>
- **`INVARIANTS.md`** new PROSE entries: <list each [NEW] invariant from above>
- **`SURFACES.md`** new rows: <list each [NEW] surface from above>
- **Issues filed:** <one per Phase above; reference the issue numbers once filed>

When the issues land and the agent-model carries the deltas, **move this file
to `designs/_archived/`**. Optionally append a one-line "Distilled: <UTC>;
shipped via <PR numbers>" line at the top of this section as a provenance trail.

---

<!--
  Template author notes (delete before publishing):
  - The Status line above tracks lifecycle: Draft → Approved (direction won) →
    Distilled (decisions + invariants + surfaces landed; issues filed) → Archived.
  - The template is a CHECKLIST shape. Sections marked TODO get filled in;
    sections not applicable (e.g. no new primitives, no grounding research)
    can be deleted.
  - The Grounding field is the explicit citation surface for research notes
    (operator-written narrative siblings in designs/) AND cycle-scoped verdicts
    (`.fctry/refs/<UTC>-<slug>.json` emitted by /fctry:ref and #153 surfaces).
    Both are CITED, not absorbed — provenance stays at source.
  - File this as `designs/<slug>.md` where <slug> is short-hyphenated and matches
    the filename used in the title heading. If the design has a sibling research
    note, name it `designs/<slug>-research.md` so they sort together.
-->
