# cascade-catalog

Real `pg_catalog` and `information_schema`.

## Role

Layer **06-catalog**. Implements §4.3 — the catalog is *not* a mock. It is a real, query-backed system catalog stored in heap tables and indexed like any other relation. ORMs and migration tools can introspect arbitrary catalog queries and get correct answers.

## Why real, not mocked

The original "Postgres Illusion" plan was to return hardcoded responses for `pg_catalog` queries. That breaks the moment an ORM joins three catalog tables in a way we didn't anticipate (Prisma, Hibernate, and Flyway do this regularly). The cost of mocking grows with every new query shape; the cost of a real catalog is paid once.

## Public API (initial)

- `Catalog` — handle. `lookup_relation`, `create_relation`, `alter_relation`, `lookup_type`, etc.
- Bootstrap — how the catalog tables themselves come into existence at first start.
- DDL handlers that produce WAL records for §6.3 logical decoding.

## Dependencies

- `cascade-heap`, `cascade-btree`, `cascade-mvcc` — catalog rows live in heap relations like everything else.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §4.3.
