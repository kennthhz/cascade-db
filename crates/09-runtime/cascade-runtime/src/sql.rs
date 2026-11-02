//! SQL front end for the debug engine: real PG-dialect parsing via
//! `cascade-pg-parser` (libpg_query — ADR 0002, the user's 2026-10-25
//! ruling), then *fixed-schema resolution* against the slice's one table
//! `debug_table(id INT, val TEXT)` and the debug function `debug_worker()`.
//!
//! The split is deliberate and is the honest shape of "parse-only ≠
//! planner": the parser (PG's grammar, byte-exact dialect) answers *what
//! the text says*; this module answers *what it means here* — the two
//! SQLSTATEs a real catalog would produce (42P01 undefined table, 42703
//! undefined column) come from comparing the parsed shape against the
//! fixed schema, not from the parser. Everything parsed-but-unmodeled is
//! refused with 0A000 (feature not supported), never mislabeled a syntax
//! error.
//!
//! Resolution results feed [`Stmt`], the same enum the session executed
//! before this was a real parser — the engine below it is unchanged.

use crate::config::NumaAffinity;
use cascade_pg_parser as pgp;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SqlError {
    /// The parser refused the text — PG's own message (e.g.
    /// `syntax error at or near "SELEC"`).
    #[error("{message}")]
    Syntax { message: String },
    #[error("relation \"{name}\" does not exist")]
    UndefinedTable { name: String },
    #[error("column \"{name}\" does not exist")]
    UndefinedColumn { name: String },
    #[error("{command} is not supported by this engine")]
    Unsupported { command: String },
    /// A well-known parameter got a value outside its domain — PG's
    /// `invalid_parameter_value` (FR-38's "parses and validates": the
    /// numa_affinity value is checked before the feature is refused).
    #[error("invalid value for parameter \"{parameter}\": \"{value}\"")]
    InvalidParameterValue { parameter: String, value: String },
    #[error("{0}")]
    Protocol(&'static str),
}

impl SqlError {
    /// SQLSTATE for ErrorResponse.
    pub fn code(&self) -> &'static str {
        match self {
            SqlError::Syntax { .. } => "42601",
            SqlError::UndefinedTable { .. } => "42P01",
            SqlError::UndefinedColumn { .. } => "42703",
            SqlError::Unsupported { .. } => "0A000",
            SqlError::InvalidParameterValue { .. } => "22023",
            SqlError::Protocol(_) => "08P01",
        }
    }
}

/// Which fixed relation a SELECT targets (the resolution layer's stand-in
/// for a catalog lookup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TableKind {
    /// The debug engine's transactional table (`debug_table(id, val)`).
    Debug,
    /// The perf demo's read-only pool-resident heap (`demo(id, payload)`,
    /// `demoheap`) — present only when booted with `demo_rows > 0`.
    Demo,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Id,
    /// `debug_table.val`.
    Val,
    /// `demo.payload`.
    Payload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// `SELECT … FROM <fixed table> [WHERE id = <i32>]` — the point
    /// filter is the slice's only predicate (the perf demo's lookup
    /// shape).
    Select {
        table: TableKind,
        cols: Vec<Col>,
        filter: Option<i32>,
    },
    DebugWorker,
    /// `SELECT debug_roles()` — the Tier C catalog's role names, read
    /// from THIS reactor's snapshot (the pointer load is the read; T5.4).
    DebugRoles,
    /// `CREATE ROLE name` — the T5.4 DDL. Execution leaves the session:
    /// the statement bounces through the Tier C writer (FR-29) and the
    /// reply comes from its ack, never from local state.
    CreateRole { name: String },
    /// `CREATE DATABASE name [WITH …]` — T5.5's affinity DDL (FR-38).
    /// Like CreateRole, execution bounces through the Tier C writer; the
    /// `numa_affinity` option (when given) becomes the row the snapshot
    /// records (`datnumaffinity`). Other WITH options are refused
    /// upstream — they change what is created and would be dropped.
    CreateDatabase {
        name: String,
        affinity: Option<NumaAffinity>,
    },
    /// `ALTER DATABASE name SET numa_affinity = …` — parsed and validated
    /// (the value must be `single`/`cross`, else 22023), then refused
    /// `unsupported_feature` (0A000): the migration runner (§2.5.4) is a
    /// Phase 7+ deliverable. Nothing bounces, nothing changes.
    AlterDatabaseAffinity { name: String, affinity: NumaAffinity },
    /// `SELECT debug_databases()` — the per-database affinity rows of
    /// THIS reactor's snapshot (the pointer load is the read; T5.5).
    DebugDatabases,
    Insert {
        id: i32,
        val: String,
    },
    Begin,
    Commit,
    Rollback,
    /// Whitespace/comment only — the session replies EmptyQueryResponse.
    Empty,
}

/// Split a simple-query batch into statements on `;` (outside quotes).
/// Wire-level text handling, upstream of the parser (libpg_query sees one
/// statement at a time, exactly like a prepared statement would).
pub fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_quote = false;
    for c in sql.chars() {
        match c {
            '\'' => {
                in_quote = !in_quote;
                cur.push(c);
            }
            ';' if !in_quote => {
                out.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur);
    }
    out
}

/// Parses one statement (no trailing `;` — callers strip it) and resolves
/// it against the fixed schema.
pub fn parse(sql: &str) -> Result<Stmt, SqlError> {
    let tree = pgp::parse(sql).map_err(|e| SqlError::Syntax { message: e.message })?;
    match tree.stmts() {
        [] => Ok(Stmt::Empty),
        [stmt] => resolve(stmt),
        [..] => Err(SqlError::Syntax {
            message: "one statement at a time — split on ';' first".into(),
        }),
    }
}

/// Fixed-schema resolution: turns the parser's syntactic view into the
/// engine's [`Stmt`], producing catalog-flavored SQLSTATEs where the real
/// PG would consult `pg_class`/`pg_attribute`.
fn resolve(stmt: &pgp::Stmt) -> Result<Stmt, SqlError> {
    match stmt {
        pgp::Stmt::Empty => Ok(Stmt::Empty),
        pgp::Stmt::Begin => Ok(Stmt::Begin),
        pgp::Stmt::Commit => Ok(Stmt::Commit),
        pgp::Stmt::Rollback => Ok(Stmt::Rollback),
        pgp::Stmt::Unsupported { command } => Err(SqlError::Unsupported {
            command: command.clone(),
        }),
        pgp::Stmt::Insert(ins) => resolve_insert(ins),
        pgp::Stmt::Select(sel) => resolve_select(sel),
        pgp::Stmt::CreateRole { name } => Ok(Stmt::CreateRole { name: name.clone() }),
        pgp::Stmt::CreateDatabase { name, options } => resolve_create_database(name, options),
        pgp::Stmt::AlterDatabaseSet {
            name,
            parameter,
            value,
        } => resolve_alter_database_set(name, parameter, value.as_ref()),
    }
}

fn resolve_insert(ins: &pgp::Insert) -> Result<Stmt, SqlError> {
    if ins.table != "debug_table" {
        return Err(SqlError::UndefinedTable { name: ins.table.clone() });
    }
    match ins.values.as_slice() {
        [pgp::Value::Int(id), pgp::Value::Text(val)] if *id >= i32::MIN as i64 && *id <= i32::MAX as i64 => {
            Ok(Stmt::Insert {
                id: *id as i32,
                val: val.clone(),
            })
        }
        _ => Err(SqlError::Unsupported { command: "insert".into() }),
    }
}

/// FR-38's creation surface. The one understood option is
/// `numa_affinity = 'single'|'cross'` — its value is validated HERE (a
/// bad value is 22023, not a generic refusal, because the caller named a
/// parameter the engine knows). PG's real creation options (OWNER,
/// TEMPLATE, …) carry semantics this engine does not model, so they are
/// refused 0A000 — creating the database without them would lie about
/// what was created. The bare form inherits the cluster default at
/// creation (the resolution is frozen into the row, PG-faithful: later
/// default changes don't reach an existing database).
fn resolve_create_database(name: &str, options: &[pgp::DbOption]) -> Result<Stmt, SqlError> {
    let affinity = match options {
        [] => None,
        [opt] if opt.name.eq_ignore_ascii_case("numa_affinity") => {
            Some(numa_affinity_value(opt.value.as_ref())?)
        }
        _ => return Err(SqlError::Unsupported { command: "create database".into() }),
    };
    Ok(Stmt::CreateDatabase {
        name: name.to_owned(),
        affinity,
    })
}

/// FR-38's honest ALTER: the `numa_affinity` parameter is recognized and
/// its value validated (22023 for garbage), and the well-formed statement
/// resolves only to be refused downstream — `unsupported_feature` until
/// the Phase 7+ migration runner (§2.5.4). Any other parameter is a GUC
/// store this engine does not have: 0A000.
fn resolve_alter_database_set(
    name: &str,
    parameter: &str,
    value: Option<&pgp::Value>,
) -> Result<Stmt, SqlError> {
    if parameter.eq_ignore_ascii_case("numa_affinity") {
        let affinity = numa_affinity_value(value)?;
        return Ok(Stmt::AlterDatabaseAffinity {
            name: name.to_owned(),
            affinity,
        });
    }
    Err(SqlError::Unsupported { command: "alter database".into() })
}

/// The `numa_affinity` option's value: the two cluster modes, or 22023
/// with the value as written (non-text values render, since the caller
/// named a real parameter with a wrong-typed value).
fn numa_affinity_value(value: Option<&pgp::Value>) -> Result<NumaAffinity, SqlError> {
    let invalid = |value: String| {
        Err(SqlError::InvalidParameterValue {
            parameter: "numa_affinity".into(),
            value,
        })
    };
    match value {
        Some(pgp::Value::Text(t)) => match t.to_ascii_lowercase().as_str() {
            "single" => Ok(NumaAffinity::Single),
            "cross" => Ok(NumaAffinity::Cross),
            _ => invalid(t.clone()),
        },
        Some(pgp::Value::Int(n)) => invalid(n.to_string()),
        Some(pgp::Value::Float(f)) => invalid(f.clone()),
        Some(pgp::Value::Bool(b)) => invalid(b.to_string()),
        Some(pgp::Value::Null) | Some(pgp::Value::Unsupported) | None => invalid("…".into()),
    }
}

fn resolve_select(sel: &pgp::Select) -> Result<Stmt, SqlError> {
    let Some(table) = &sel.from else {
        // A bare SELECT (no FROM): the slice knows two — the debug
        // function calls. `SELECT 1` and friends parse; they are just
        // not executable here.
        return match (sel.targets.as_slice(), &sel.filter) {
            ([pgp::Target::Function { name }], None)
                if name.eq_ignore_ascii_case("debug_worker") =>
            {
                Ok(Stmt::DebugWorker)
            }
            ([pgp::Target::Function { name }], None)
                if name.eq_ignore_ascii_case("debug_roles") =>
            {
                Ok(Stmt::DebugRoles)
            }
            ([pgp::Target::Function { name }], None)
                if name.eq_ignore_ascii_case("debug_databases") =>
            {
                Ok(Stmt::DebugDatabases)
            }
            _ => Err(SqlError::Unsupported { command: "select".into() }),
        };
    };

    // Catalog stand-in: which fixed relation, and which columns exist on
    // it. A real catalog arrives with Phase 3.
    let kind = match table.name.as_str() {
        "debug_table" => TableKind::Debug,
        "demo" => TableKind::Demo,
        other => return Err(SqlError::UndefinedTable { name: other.to_owned() }),
    };

    // Target list → projection. `*` expands to the whole fixed schema and
    // must stand alone (PG would allow `SELECT *, id` — unmodeled).
    let mut cols = Vec::new();
    for target in &sel.targets {
        match target {
            pgp::Target::Star => {
                if !cols.is_empty() {
                    return Err(SqlError::Unsupported { command: "select".into() });
                }
                match kind {
                    TableKind::Debug => cols.extend([Col::Id, Col::Val]),
                    TableKind::Demo => cols.extend([Col::Id, Col::Payload]),
                }
            }
            pgp::Target::Column(name) => match (kind, name.as_str()) {
                (TableKind::Debug, "id") => cols.push(Col::Id),
                (TableKind::Debug, "val") => cols.push(Col::Val),
                (TableKind::Demo, "id") => cols.push(Col::Id),
                (TableKind::Demo, "payload") => cols.push(Col::Payload),
                // Cross-table columns are exactly what a catalog would
                // call undefined.
                (_, other) => return Err(SqlError::UndefinedColumn { name: other.to_owned() }),
            },
            pgp::Target::Function { .. } | pgp::Target::Unsupported => {
                return Err(SqlError::Unsupported { command: "select".into() });
            }
        }
    }

    let filter = match &sel.filter {
        None => None,
        Some(pgp::Condition::Eq { left, right }) => {
            // Normalize either written order to (column, constant).
            let (column, value) = match (left, right) {
                (pgp::Operand::Column(c), pgp::Operand::Const(v))
                | (pgp::Operand::Const(v), pgp::Operand::Column(c)) => (c, v),
                _ => return Err(SqlError::Unsupported { command: "select".into() }),
            };
            let col = column.to_ascii_lowercase();
            match (col.as_str(), value) {
                ("id", pgp::Value::Int(n))
                    if *n >= i32::MIN as i64 && *n <= i32::MAX as i64 =>
                {
                    Some(*n as i32)
                }
                // Known column, unmodelable comparison (e.g. val = 'x'):
                // unsupported, not a column error.
                ("id", _) | ("val", _) => {
                    Err(SqlError::Unsupported { command: "select".into() })?
                }
                // Not a column of the fixed schema: the 42703 a real
                // catalog would raise.
                (_, _) => Err(SqlError::UndefinedColumn { name: column.clone() })?,
            }
        }
        Some(_) => return Err(SqlError::Unsupported { command: "select".into() }),
    };

    Ok(Stmt::Select { table: kind, cols, filter })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_subset() {
        assert_eq!(
            parse("SELECT * FROM debug_table"),
            Ok(Stmt::Select { table: TableKind::Debug, cols: vec![Col::Id, Col::Val], filter: None })
        );
        assert_eq!(
            parse("select id, val from debug_table"),
            Ok(Stmt::Select { table: TableKind::Debug, cols: vec![Col::Id, Col::Val], filter: None })
        );
        assert_eq!(
            parse("SELECT val FROM debug_table"),
            Ok(Stmt::Select { table: TableKind::Debug, cols: vec![Col::Val], filter: None })
        );
        // The demo relation resolves on the same terms.
        assert_eq!(
            parse("SELECT payload FROM demo WHERE id = 5"),
            Ok(Stmt::Select {
                table: TableKind::Demo,
                cols: vec![Col::Payload],
                filter: Some(5)
            })
        );
        assert_eq!(
            parse("SELECT val FROM demo").unwrap_err().code(),
            "42703",
            "debug_table's columns do not exist on demo"
        );
        assert_eq!(parse("SELECT debug_worker()"), Ok(Stmt::DebugWorker));
        assert_eq!(parse("select DEBUG_ROLES()"), Ok(Stmt::DebugRoles));
        // The T5.4 DDL resolves to the engine statement the session
        // bounces through the Tier C writer.
        assert_eq!(
            parse("CREATE ROLE alice"),
            Ok(Stmt::CreateRole { name: "alice".into() })
        );
        // Options are refused upstream as 0A000 (parsed, unmodeled) —
        // the parser's boundary, restated here as a code pin.
        assert_eq!(parse("CREATE ROLE alice WITH LOGIN").unwrap_err().code(), "0A000");
        // T5.5: the affinity DDL resolves — the numa_affinity option
        // becomes the snapshot row (datnumaffinity), the bare form
        // inherits, garbage values are 22023 (validated, then refused),
        // and PG's real creation options stay 0A000.
        assert_eq!(
            parse("CREATE DATABASE oltp WITH numa_affinity = 'single'"),
            Ok(Stmt::CreateDatabase { name: "oltp".into(), affinity: Some(NumaAffinity::Single) })
        );
        assert_eq!(
            parse("CREATE DATABASE bulk WITH numa_affinity = 'cross'"),
            Ok(Stmt::CreateDatabase { name: "bulk".into(), affinity: Some(NumaAffinity::Cross) })
        );
        assert_eq!(
            parse("CREATE DATABASE plain"),
            Ok(Stmt::CreateDatabase { name: "plain".into(), affinity: None })
        );
        assert_eq!(
            parse("CREATE DATABASE bad WITH numa_affinity = 'sideways'")
                .unwrap_err()
                .code(),
            "22023",
            "the value is validated before the feature question"
        );
        assert_eq!(
            parse("CREATE DATABASE owned WITH OWNER = 'alice'").unwrap_err().code(),
            "0A000",
            "unmodeled creation options are refused, never dropped"
        );
        // ALTER validates then resolves — the session refuses it with
        // unsupported_feature; nothing here bounces or mutates.
        assert_eq!(
            parse("ALTER DATABASE oltp SET numa_affinity = 'cross'"),
            Ok(Stmt::AlterDatabaseAffinity { name: "oltp".into(), affinity: NumaAffinity::Cross })
        );
        assert_eq!(
            parse("ALTER DATABASE oltp SET numa_affinity = 'wrong'").unwrap_err().code(),
            "22023"
        );
        assert_eq!(
            parse("ALTER DATABASE oltp SET work_mem = '64MB'").unwrap_err().code(),
            "0A000"
        );
        assert_eq!(parse("SELECT debug_databases()"), Ok(Stmt::DebugDatabases));
        assert_eq!(
            parse("INSERT INTO debug_table VALUES (1, 'hello')"),
            Ok(Stmt::Insert { id: 1, val: "hello".into() })
        );
        assert_eq!(
            parse("insert into debug_table values (-2, 'it''s')"),
            Ok(Stmt::Insert { id: -2, val: "it's".into() })
        );
        assert_eq!(parse("BEGIN"), Ok(Stmt::Begin));
        assert_eq!(parse("COMMIT"), Ok(Stmt::Commit));
        assert_eq!(parse("ROLLBACK"), Ok(Stmt::Rollback));
        assert_eq!(parse("   "), Ok(Stmt::Empty));
        assert_eq!(
            split_statements("SELECT 1; INSERT INTO debug_table VALUES (1, 'a;b')"),
            vec![
                "SELECT 1".to_owned(),
                " INSERT INTO debug_table VALUES (1, 'a;b')".to_owned()
            ]
        );
    }

    #[test]
    fn resolves_the_point_filter() {
        // The demo's query shape: a point lookup by id, either written
        // order, negative constants folded by the grammar.
        assert_eq!(
            parse("SELECT val FROM debug_table WHERE id = 42"),
            Ok(Stmt::Select { table: TableKind::Debug, cols: vec![Col::Val], filter: Some(42) })
        );
        assert_eq!(
            parse("SELECT * FROM debug_table WHERE 42 = id"),
            Ok(Stmt::Select { table: TableKind::Debug, cols: vec![Col::Id, Col::Val], filter: Some(42) })
        );
        assert_eq!(
            parse("SELECT id FROM debug_table WHERE id = -7"),
            Ok(Stmt::Select { table: TableKind::Debug, cols: vec![Col::Id], filter: Some(-7) })
        );
    }

    #[test]
    fn rejects_with_pg_error_codes() {
        // 42P01/42703: the fixed schema plays the catalog's role.
        assert_eq!(parse("SELECT * FROM other").unwrap_err().code(), "42P01");
        assert_eq!(
            parse("SELECT nope FROM debug_table").unwrap_err().code(),
            "42703"
        );
        assert_eq!(
            parse("INSERT INTO other VALUES (1, 'x')").unwrap_err().code(),
            "42P01"
        );
        // 0A000: parsed cleanly, outside the modeled surface.
        assert_eq!(parse("DROP TABLE users").unwrap_err().code(), "0A000");
        assert_eq!(parse("UPDATE debug_table SET id = 2").unwrap_err().code(), "0A000");
        assert_eq!(
            parse("SELECT * FROM debug_table WHERE val = 'x'").unwrap_err().code(),
            "0A000",
            "typed predicates are unmodeled"
        );
        assert_eq!(
            parse("SELECT * FROM debug_table WHERE id > 1").unwrap_err().code(),
            "0A000"
        );
        assert_eq!(
            parse("SELECT * FROM debug_table WHERE x=1").unwrap_err().code(),
            "42703",
            "unknown filter column is a column error, not syntax"
        );
        assert_eq!(
            parse("SELECT * FROM debug_table WHERE id = 'x'").unwrap_err().code(),
            "0A000"
        );
        assert_eq!(parse("SELECT 1").unwrap_err().code(), "0A000");
    }

    #[test]
    fn syntax_errors_carry_pg_text() {
        let err = parse("SELEC 1").unwrap_err();
        assert_eq!(err.code(), "42601");
        assert!(
            err.to_string().contains("syntax error at or near"),
            "PG's own wording: {err}"
        );
    }
}
