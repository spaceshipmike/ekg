# Surfaces

This project ships through multiple surfaces, and several depend on each other in
non-obvious ways. Before calling a change "done," sweep this list and ask which
surfaces it touches. Most changes touch one or two; the dangerous ones touch a
cross-cutting edge without looking like they do.

## The map

| Surface | Lives in | A change ripples here when you… | Verify |
|---|---|---|---|
| <!-- prompt: name one surface (web app, REST API, CLI, MCP server, extension, marketing site, etc.) --> | <!-- code path or repo --> | <!-- trigger condition --> | <!-- how to verify it still works --> |

<!-- prompt: add one row per surface. See knowmarks/SURFACES.md for the proven shape of
     a populated map. -->

## Cross-cutting edges (the silent ones)

<!-- prompt: each bullet names ONE non-obvious cross-surface dependency — the kind a
     reasonable change would silently break. Concrete: file → file, surface → surface,
     contract → consumers. -->

- <!-- example bullet — replace -->

## The sweep

<!-- prompt: a short checklist of "did you check X, Y, Z" — the things that catch the
     dangerous changes. The thing you read before merging. -->
