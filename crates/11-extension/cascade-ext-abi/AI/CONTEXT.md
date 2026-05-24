# cascade-ext-abi — AI Design Context

## Role

The extension-hosting layer.

## Pillar Reservations

- **§4.4:** the entire reason this crate exists.
- **§3.6 (Rust):** hosting upstream C extensions is a second FFI boundary (after libpg_query). Bigger surface, more dangerous. Audit boundary lives here.

## Hard Invariants

1. **Extension types are first-class.** No special-casing in the planner or executor for extension-provided types.
2. **C-hosted extensions are sandboxed** as much as feasible — restricted from touching internal storage APIs, restricted to the published ABI.
3. **Per-database load.** An extension loaded into database A is not implicitly available in database B.

## Out of Scope

- Specific extension implementations (pgvector, PostGIS) — those live in their own `cascade-ext-*` crates.
- Sandboxing technology (seccomp, namespaces) — Phase 5+ topic.

## Open Design Questions

- ABI shape: mimic PG's extension ABI verbatim (maximize compatibility), define our own (cleaner but blocks reuse of upstream extension source).
- Whether we ship a PG-extension *binary* compatibility layer (load upstream `.so` directly) — extremely valuable but very hard.
