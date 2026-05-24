# ADR 0001 — Flat workspace layout with layered crate directories

**Status:** Accepted
**Date:** 2026-05-24

## Context

The codebase has ~25 components. Two ways to encode dependency relationships on disk were considered:

1. **Nested** — `cascade-bpm/cascade-storage/` so the directory tree mirrors the call graph.
2. **Flat under `crates/`** — every crate is a sibling, dependencies expressed in `Cargo.toml`.

## Decision

Use **flat crates under `crates/NN-layer/`** with numeric layer prefixes to make dependency direction visually obvious.

## Consequences

- Dependency graph is expressed *exactly once*, in Cargo.toml, and enforced by the compiler.
- Directory structure cannot lie. Nesting was rejected because `cascade-storage` is used by many peers (BPM, WAL, columnar, recovery, future page service); putting it inside any one of them would falsely imply ownership.
- Layer prefixes (`01-foundation`, `02-storage`, …) are a code-review convention, not Cargo-enforced. Layer `N` may depend on `1..N`, never on `N+1..`.

## Alternatives considered

- **Nested by dependent.** Rejected — see above.
- **Flat without layer prefixes.** Rejected — loses at-a-glance dependency direction in a 25-crate workspace.
- **Single crate with internal modules.** Rejected — denies us compile-time API boundaries between subsystems and slows builds.
