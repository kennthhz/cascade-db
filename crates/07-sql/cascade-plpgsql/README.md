# cascade-plpgsql

PL/pgSQL procedural language runtime.

## Role

Layer **07-sql**. Implements the procedural language ORMs and existing applications depend on for functions, stored procedures, and triggers (§4.3).

## Public API (initial)

- `compile(source) -> PlPgSqlFn` — parse + bind a function body.
- `execute(fn, args, session) -> Value` — runtime interpreter.
- Trigger dispatch hooks for `cascade-sql-exec`.

## Dependencies

- `cascade-pg-parser` — body parsing.
- `cascade-pgtypes` — variable types.
- `cascade-sql-planner` + `cascade-sql-exec` — embedded SQL execution within a function body.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §4.3.
