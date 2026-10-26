-- The demo query: one point select, identical text on both sides.
-- PG: parse → plan (cached if -M prepared) → pkey index scan → heap tuple.
-- cascade: libpg_query parse → fixed-schema resolution → pool-resident row.
\set id random(1, 100000)
SELECT payload FROM demo WHERE id = :id;
