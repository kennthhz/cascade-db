# cascade-pgtypes

PostgreSQL type system — values, coercion rules, wire-format codecs.

## Role

Layer **07-sql**. Implements §4.2: the PG type system. Binary and text representations match PG byte-for-byte on the wire, so unmodified PG clients decode values the same way they always have.

## What lives here

- Type registry: `int2`, `int4`, `int8`, `numeric`, `text`, `varchar`, `date`, `time`, `timestamp`, `timestamptz`, `interval`, `uuid`, `bytea`, `json`, `jsonb`, `inet`, `cidr`, `point`, arrays, ranges, enums, composites, …
- Wire-format codecs (PG binary + text representations).
- Coercion / cast rules.
- Operator tables (`+`, `-`, `=`, comparison, jsonb operators, …).

## Dependencies

- `cascade-types`, `cascade-error`.
- Eventually: `chrono` / `jiff` for date/time math, `rust_decimal` or hand-rolled numeric, `serde_json` for jsonb.

## See also

- [AI/CONTEXT.md](AI/CONTEXT.md)
- README §4.2.
