# Build Learnings

> Cross-session build learnings for this project. Append-only, git-tracked.
>
> **Methodology entries** use `#fctry-core` as the alias (no section number).
> These travel into the fctry plugin via the cross-repo `fctry-core` harvest.
>
> Entry format:
> ### {ISO 8601 timestamp} | #{alias} ({optional section-number})
> **Status:** candidate | **Confidence:** 1
> **Trigger:** {failure-rearchitect | retry-success | tech-stack-pattern | experience-question}
> **Context:** {What was attempted}
> **Outcome:** {What failed or succeeded}
> **Lesson:** {What to do differently next time}
>
> Maturation lifecycle: candidate (confidence 1) → active (confidence 3+).
> Only active lessons influence builds.

---
### 2026-07-25T21:35:00Z | #ekg-shell-portability
**Status:** candidate | **Confidence:** 1
**Trigger:** failure-rearchitect
**Context:** Hook-timeout wrapper (PR #11) used `kill -TERM -- -$pgid` to group-kill the hook process tree. Green locally and on macOS CI, failed on Ubuntu CI.
**Outcome:** dash (Ubuntu's /bin/sh) builtin `kill` cannot parse the `--` end-of-options marker before a negative pgid — "Illegal number: -". The kill silently failed, breaking the whole-tree timeout guarantee on most Linux distros. Fix: drop `--` (`kill -TERM -$pgid`), which every implementation supports.
**Lesson:** Any `sh -c` script ekg ships must be verified under dash, not just bash — macOS dev machines hide dash-isms. A local dash exists at /bin/dash on macOS; test wrapper scripts through it explicitly (there is now a regression test doing exactly this in src/hooks.rs).

### 2026-07-25T21:35:00Z | #fctry-core
**Status:** candidate | **Confidence:** 1
**Trigger:** retry-success
**Context:** PRs #10/#11 went through 3 and 6 Codex review rounds respectively; every round's blockers were real (loss-stat dilution, fail-open validation, terminal corruption, process-group escapes, dash portability).
**Outcome:** The review loop converged; the merged hook implementation survives Ctrl-C, bounds hung hooks without ekg alive, and works under dash. Subprocess-lifecycle code (spawn/detach/timeout/kill) reliably needs multiple adversarial review rounds — first-pass "detached with timeout" implementations are essentially always wrong at the edges.
**Lesson:** For process-lifecycle features, budget for repeated Codex rounds and demand tests that probe the tree (group membership, TERM-immune children, cross-shell), not just the direct child.
