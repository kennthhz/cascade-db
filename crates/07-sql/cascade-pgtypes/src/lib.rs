//! PostgreSQL type system.
//!
//! See `README.md` and `AI/CONTEXT.md`.

#![allow(dead_code)]

/// PG type OID — matches PostgreSQL's reserved range for built-in types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TypeOid(pub u32);

/// Well-known type OIDs (match upstream PG).
pub mod oid {
    use super::TypeOid;
    pub const INT2:       TypeOid = TypeOid(21);
    pub const INT4:       TypeOid = TypeOid(23);
    pub const INT8:       TypeOid = TypeOid(20);
    pub const TEXT:       TypeOid = TypeOid(25);
    pub const NUMERIC:    TypeOid = TypeOid(1700);
    pub const TIMESTAMP:  TypeOid = TypeOid(1114);
    pub const TIMESTAMPTZ:TypeOid = TypeOid(1184);
    pub const UUID:       TypeOid = TypeOid(2950);
    pub const JSON:       TypeOid = TypeOid(114);
    pub const JSONB:      TypeOid = TypeOid(3802);
    pub const BYTEA:      TypeOid = TypeOid(17);
    // ... etc.
}
