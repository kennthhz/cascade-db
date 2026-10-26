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
            SqlError::Protocol(_) => "08P01",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Col {
    Id,
    Val,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stmt {
    /// `SELECT … FROM debug_table [WHERE id = <i32>]` — the point filter
    /// is the slice's only predicate (the perf demo's lookup shape).
    Select {
        cols: Vec<Col>,
        filter: Option<i32>,
    },
    DebugWorker,
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

fn resolve_select(sel: &pgp::Select) -> Result<Stmt, SqlError> {
    let Some(table) = &sel.from else {
        // A bare SELECT (no FROM): the slice knows one — the debug
        // function call. `SELECT 1` and friends parse; they are just not
        // executable here.
        return match (sel.targets.as_slice(), &sel.filter) {
            ([pgp::Target::Function { name }], None)
                if name.eq_ignore_ascii_case("debug_worker") =>
            {
                Ok(Stmt::DebugWorker)
            }
            _ => Err(SqlError::Unsupported { command: "select".into() }),
        };
    };

    if table.name != "debug_table" {
        return Err(SqlError::UndefinedTable { name: table.name.clone() });
    }

    // Target list → projection. `*` expands to the whole fixed schema and
    // must stand alone (PG would allow `SELECT *, id` — unmodeled).
    let mut cols = Vec::new();
    for target in &sel.targets {
        match target {
            pgp::Target::Star => {
                if !cols.is_empty() {
                    return Err(SqlError::Unsupported { command: "select".into() });
                }
                cols.extend([Col::Id, Col::Val]);
            }
            pgp::Target::Column(name) => match name.as_str() {
                "id" | "ID" => cols.push(Col::Id),
                "val" | "VAL" => cols.push(Col::Val),
                _ => return Err(SqlError::UndefinedColumn { name: name.clone() }),
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

    Ok(Stmt::Select { cols, filter })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_the_subset() {
        assert_eq!(
            parse("SELECT * FROM debug_table"),
            Ok(Stmt::Select { cols: vec![Col::Id, Col::Val], filter: None })
        );
        assert_eq!(
            parse("select id, val from debug_table"),
            Ok(Stmt::Select { cols: vec![Col::Id, Col::Val], filter: None })
        );
        assert_eq!(
            parse("SELECT val FROM debug_table"),
            Ok(Stmt::Select { cols: vec![Col::Val], filter: None })
        );
        assert_eq!(parse("SELECT debug_worker()"), Ok(Stmt::DebugWorker));
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
            Ok(Stmt::Select { cols: vec![Col::Val], filter: Some(42) })
        );
        assert_eq!(
            parse("SELECT * FROM debug_table WHERE 42 = id"),
            Ok(Stmt::Select { cols: vec![Col::Id, Col::Val], filter: Some(42) })
        );
        assert_eq!(
            parse("SELECT id FROM debug_table WHERE id = -7"),
            Ok(Stmt::Select { cols: vec![Col::Id], filter: Some(-7) })
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
