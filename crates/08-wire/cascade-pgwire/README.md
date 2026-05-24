# cascade-pgwire

PostgreSQL TCP wire protocol implementation.

## Role

Layer **08-wire**. Implements §4.1 — speaks the PostgreSQL TCP wire protocol on port 5432 so every PG driver (`libpq`, JDBC, `pgx`, `psycopg`, …) works unmodified.

## What's covered

- Simple and extended query protocols.
- Prepared statements, portals.
- `COPY` (text + binary).
- Cursors, `LISTEN` / `NOTIFY`.
- Authentication: `SCRAM-SHA-256`, `MD5`, TLS/SSL.

## Dependencies

- `cascade-pgtypes` — wire-format encoding/decoding of values.
- `cascade-sql-planner` + `cascade-sql-exec` — what runs once a query arrives.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §4.1.
