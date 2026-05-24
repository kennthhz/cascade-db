# cascade-catalog — AI Design Context

## Role

Real `pg_catalog` + `information_schema`. The compatibility-critical layer for ORMs and migration tools.

## Pillar Reservations

- **§4.3:** the entire reason this crate exists. Real, query-backed catalog.
- **§6.3 (CDC):** DDL operations emit WAL records so logical decoders can interpret historical row formats.

## Hard Invariants

1. **Catalog rows live in heap tables.** No special storage path. The catalog is bootstrap-aware code over normal infrastructure.
2. **Catalog OIDs are stable.** Once assigned, an object's OID does not change for its lifetime.
3. **PG-compatible system OIDs** for well-known objects (built-in types, system functions) — match PG's reserved range so client tools that hardcode OIDs work.
4. **DDL is transactional.** A failed `CREATE TABLE` leaves no orphan rows.
5. **DDL is WAL-logged** for §6.3.

## Out of Scope

- Catalog-cache layer for hot lookups — a Phase 5 optimization, not a v1 invariant.
- Multi-version catalog (online schema change) — design must reserve room, implementation later.

## Open Design Questions

- Bootstrap sequence: how the catalog tables come into existence at first start.
- OID assignment: copy PG's exact built-in OIDs (compatibility) or assign our own with a translation map. Lean toward exact match for well-known types.
- `information_schema` — implemented as views over `pg_catalog`? PG does this; we can too.
