//! Safe Rust view over libpg_query's parse trees — the boundary between
//! the FFI exception (§3.6, ADR 0002) and the rest of the engine.
//!
//! ## What this is
//!
//! [`parse`] runs the string through PostgreSQL's real grammar (via
//! [`cascade_pg_parser_sys`]) and converts the protobuf tree into owned
//! Rust types: no raw pointers, no C lifetimes, `Send + Sync` throughout.
//! The view is *syntactic only* — it mirrors what the grammar saw (table
//! name, target list, a `column = constant` predicate shape), it resolves
//! nothing: no name lookup, no types, no planning. Those stay with the
//! engine (the debug engine resolves against its fixed schema; the real
//! catalog arrives with Phase 3).
//!
//! ## Bounded view, honestly bounded
//!
//! `Stmt::Unsupported` is a load-bearing variant: a statement that parsed
//! cleanly but exceeds the modeled surface comes back as `Unsupported`
//! naming the command, so callers refuse with *feature not supported*
//! (0A000) instead of pretending the text was malformed. The same applies
//! inside SELECTs ([`Target::Unsupported`], [`Condition::Unsupported`]).
//! Growing the surface is then a matter of modeling more shapes — the
//! grammar is already total.
//!
//! Parse-only ≠ planner, and this crate never implies otherwise: PG pairs
//! this exact parse with analysis + planning; cascade pairs it with
//! whatever the caller does next (for the perf demo, a fixed-schema
//! lookup). Both sides pay the same frontend parse cost; the comparison
//! states the residual gap rather than hiding it.

use cascade_pg_parser_sys as sys;
use sys::pb::{self, node::Node, AExprKind, SetOperation, TransactionStmtKind};

/// A parse failure: PG's own error text (e.g. `syntax error at or near
/// "SELEC"` — the string PG's parser produced, wording and position
/// summary included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
}

/// One modeled statement shape. See the crate doc for `Unsupported`'s role.
#[derive(Debug, Clone, PartialEq)]
pub enum Stmt {
    Select(Select),
    Insert(Insert),
    Begin,
    Commit,
    Rollback,
    /// Whitespace/comment only — no statements in the string.
    Empty,
    /// Parsed cleanly, outside the modeled surface (DROP, UPDATE, UNION,
    /// …). `command` is a short lowercase name for error messages.
    Unsupported { command: String },
}

/// `SELECT … [FROM …] [WHERE …]` — the slice's read shape.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Select {
    /// The FROM target, when present. `None` = a bare `SELECT` (constants,
    /// function calls).
    pub from: Option<Table>,
    pub targets: Vec<Target>,
    /// The WHERE predicate shape, when present.
    pub filter: Option<Condition>,
}

/// A FROM target.
#[derive(Debug, Clone, PartialEq)]
pub struct Table {
    pub name: String,
}

/// One select-list item.
#[derive(Debug, Clone, PartialEq)]
pub enum Target {
    /// A bare column reference (`val`, `id`).
    Column(String),
    /// `*`.
    Star,
    /// A no-argument function call (`debug_worker()`).
    Function { name: String },
    /// Any other select-list expression — parsed, unmodeled.
    Unsupported,
}

/// A WHERE predicate shape.
#[derive(Debug, Clone, PartialEq)]
pub enum Condition {
    /// `a = b` in either written order (PG keeps the text's order; the
    /// caller normalizes).
    Eq { left: Operand, right: Operand },
    /// Any other predicate — parsed, unmodeled.
    Unsupported,
}

/// One side of a [`Condition::Eq`].
#[derive(Debug, Clone, PartialEq)]
pub enum Operand {
    Column(String),
    Const(Value),
    /// Parsed, unmodeled.
    Unsupported,
}

/// A literal constant.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Int(i64),
    Float(String),
    Text(String),
    Bool(bool),
    Null,
    /// Parsed, unmodeled (bit strings, typed literals, …).
    Unsupported,
}

/// `INSERT INTO table VALUES (…)` — single-row slice.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    pub table: String,
    /// The one VALUES tuple, positionally.
    pub values: Vec<Value>,
}

/// A parsed string: usually one statement, never more than the caller's
/// own statement split produced.
#[derive(Debug, Clone, PartialEq)]
pub struct ParseTree {
    stmts: Vec<Stmt>,
}

impl ParseTree {
    /// The parsed statements (empty for whitespace/comment-only strings).
    pub fn stmts(&self) -> &[Stmt] {
        &self.stmts
    }
}

/// The oneof inside a list-element node (`Vec<pb::Node>` holds the
/// wrapper struct; the variant lives one `.node` down).
fn inner(n: &pb::Node) -> Option<&Node> {
    n.node.as_ref()
}

/// A `column`-or-`constant` node, or `Unsupported` for anything else.
fn operand(node: &pb::Node) -> Operand {
    let Some(variant) = node.node.as_ref() else {
        return Operand::Unsupported;
    };
    match variant {
        Node::ColumnRef(col) => match col.fields.as_slice() {
            [one] => match inner(one) {
                Some(Node::String(s)) => Operand::Column(s.sval.clone()),
                _ => Operand::Unsupported,
            },
            _ => Operand::Unsupported,
        },
        Node::AConst(c) => Operand::Const(const_value(c)),
        _ => Operand::Unsupported,
    }
}

fn const_value(c: &pb::AConst) -> Value {
    if c.isnull {
        return Value::Null;
    }
    match c.val.as_ref() {
        Some(pb::a_const::Val::Ival(i)) => Value::Int(i.ival as i64),
        Some(pb::a_const::Val::Fval(f)) => Value::Float(f.fval.clone()),
        Some(pb::a_const::Val::Sval(s)) => Value::Text(s.sval.clone()),
        Some(pb::a_const::Val::Boolval(b)) => Value::Bool(b.boolval),
        _ => Value::Unsupported,
    }
}

/// Parses with PostgreSQL's grammar and lowers to the owned view.
pub fn parse(sql: &str) -> Result<ParseTree, ParseError> {
    let raw = sys::parse(sql).map_err(|e| ParseError { message: e.message })?;
    let stmts = raw.stmts.iter().map(lower_stmt).collect();
    Ok(ParseTree { stmts })
}

fn lower_stmt(raw: &pb::RawStmt) -> Stmt {
    let Some(node) = raw.stmt.as_deref().and_then(|n| n.node.as_ref()) else {
        return Stmt::Empty;
    };
    match node {
        Node::SelectStmt(sel) => lower_select(sel),
        Node::InsertStmt(ins) => lower_insert(ins),
        Node::TransactionStmt(txn) => match txn.kind() {
            TransactionStmtKind::TransStmtBegin | TransactionStmtKind::TransStmtStart => Stmt::Begin,
            TransactionStmtKind::TransStmtCommit => Stmt::Commit,
            TransactionStmtKind::TransStmtRollback => Stmt::Rollback,
            _ => Stmt::Unsupported {
                command: "transaction statement".into(),
            },
        },
        other => Stmt::Unsupported {
            command: unsupported_command(other).into(),
        },
    }
}

/// A short lowercase command name for `Unsupported` — for error messages,
/// not for dispatch.
fn unsupported_command(node: &Node) -> &'static str {
    match node {
        Node::UpdateStmt(_) => "update",
        Node::DeleteStmt(_) => "delete",
        Node::DropStmt(_) => "drop",
        Node::CreateStmt(_) | Node::CreateSchemaStmt(_) | Node::CreateTableAsStmt(_) => "create",
        Node::AlterTableStmt(_) => "alter",
        Node::TruncateStmt(_) => "truncate",
        Node::CopyStmt(_) => "copy",
        Node::ExplainStmt(_) => "explain",
        Node::MergeStmt(_) => "merge",
        Node::VariableSetStmt(_) => "set",
        Node::VariableShowStmt(_) => "show",
        Node::LockStmt(_) => "lock",
        Node::GrantStmt(_) => "grant",
        _ => "statement",
    }
}

/// The one select shape every clause check here guards: a plain query
/// block — no set operations, and none of the clauses the view does not
/// model (GROUP/HAVING/ORDER/LIMIT/DISTINCT/WINDOW/locking/CTEs/VALUES).
fn plain_select_guard(sel: &pb::SelectStmt) -> bool {
    sel.op() == SetOperation::SetopNone
        && sel.larg.is_none()
        && sel.rarg.is_none()
        && sel.distinct_clause.is_empty()
        && sel.group_clause.is_empty()
        && sel.having_clause.is_none()
        && sel.window_clause.is_empty()
        && sel.sort_clause.is_empty()
        && sel.limit_count.is_none()
        && sel.limit_offset.is_none()
        && sel.locking_clause.is_empty()
        && sel.with_clause.is_none()
        && sel.into_clause.is_none()
}

fn lower_select(sel: &pb::SelectStmt) -> Stmt {
    let unsupported = || Stmt::Unsupported { command: "select".into() };
    if !plain_select_guard(sel) {
        return unsupported();
    }

    let from = match sel.from_clause.as_slice() {
        [] => None,
        [one] => match one.node.as_ref() {
            Some(Node::RangeVar(rv)) => Some(Table { name: rv.relname.clone() }),
            _ => return unsupported(),
        },
        [..] => return unsupported(),
    };

    let mut targets = Vec::with_capacity(sel.target_list.len());
    for item in &sel.target_list {
        targets.push(match item.node.as_ref() {
            Some(Node::ResTarget(rt)) => match rt.val.as_deref().and_then(|v| v.node.as_ref()) {
                Some(Node::ColumnRef(col)) => match col.fields.as_slice() {
                    [one] => match inner(one) {
                        Some(Node::String(s)) => Target::Column(s.sval.clone()),
                        Some(Node::AStar(_)) => Target::Star,
                        _ => Target::Unsupported,
                    },
                    _ => Target::Unsupported,
                },
                // A zero-argument plain function call (`debug_worker()`).
                Some(Node::FuncCall(f))
                    if f.args.is_empty() && !f.agg_star && f.over.is_none() =>
                {
                    match f.funcname.as_slice() {
                        [one] => match inner(one) {
                            Some(Node::String(s)) => Target::Function { name: s.sval.clone() },
                            _ => Target::Unsupported,
                        },
                        _ => Target::Unsupported,
                    }
                }
                _ => Target::Unsupported,
            },
            _ => Target::Unsupported,
        });
    }

    let filter = match sel.where_clause.as_deref().and_then(|w| w.node.as_ref()) {
        None => None,
        Some(Node::AExpr(a)) if a.kind() == AExprKind::AexprOp => {
            // `name` is the operator list; a plain `=` is one String node
            // (schema-qualified operators are two — unmodeled).
            let [one] = a.name.as_slice() else {
                return unsupported();
            };
            let Some(Node::String(op)) = inner(one) else {
                return unsupported();
            };
            if op.sval != "=" {
                return unsupported();
            }
            let (Some(l), Some(r)) = (a.lexpr.as_deref(), a.rexpr.as_deref()) else {
                return unsupported();
            };
            Some(Condition::Eq {
                left: operand(l),
                right: operand(r),
            })
        }
        Some(_) => Some(Condition::Unsupported),
    };

    Stmt::Select(Select { from, targets, filter })
}

fn lower_insert(ins: &pb::InsertStmt) -> Stmt {
    let unsupported = || Stmt::Unsupported { command: "insert".into() };
    let Some(relation) = &ins.relation else {
        return unsupported();
    };
    // The slice's INSERT shape: `VALUES (…)` with exactly one tuple (a
    // `selectStmt` holding a VALUES-lists select). Column lists are fine —
    // the mapping stays positional. Everything else → Unsupported.
    let Some(values) = (|| {
        let sel = match ins.select_stmt.as_deref()?.node.as_ref()? {
            Node::SelectStmt(sel) => sel,
            _ => return None,
        };
        if !plain_select_guard(sel) || sel.values_lists.len() != 1 {
            return None;
        }
        let Node::List(list) = sel.values_lists[0].node.as_ref()? else {
            return None;
        };
        list.items
            .iter()
            .map(|item| match item.node.as_ref() {
                Some(Node::AConst(c)) => const_value(c),
                _ => Value::Unsupported,
            })
            .collect::<Vec<_>>()
            .into()
    })() else {
        return unsupported();
    };
    Stmt::Insert(Insert {
        table: relation.relname.clone(),
        values,
    })
}

// Parse trees are plain owned data — the runtime hands them to reactor
// tasks (§3.2), so movable-across-threads is a compile-enforced property.
const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<ParseTree>();
    assert_send_sync::<Stmt>();
    assert_send_sync::<ParseError>();
};

#[cfg(test)]
mod tests {
    use super::*;

    fn one(sql: &str) -> Stmt {
        let tree = parse(sql).expect("parses");
        assert_eq!(tree.stmts().len(), 1);
        tree.stmts()[0].clone()
    }

    fn select_of(stmt: &Stmt) -> &Select {
        match stmt {
            Stmt::Select(sel) => sel,
            other => panic!("not a select: {other:?}"),
        }
    }

    #[test]
    fn select_shapes() {
        assert_eq!(
            one("SELECT * FROM debug_table"),
            Stmt::Select(Select {
                from: Some(Table { name: "debug_table".into() }),
                targets: vec![Target::Star],
                filter: None,
            })
        );
        assert_eq!(
            one("select id, val from debug_table"),
            Stmt::Select(Select {
                from: Some(Table { name: "debug_table".into() }),
                targets: vec![Target::Column("id".into()), Target::Column("val".into())],
                filter: None,
            })
        );
        assert_eq!(
            one("SELECT debug_worker()"),
            Stmt::Select(Select {
                from: None,
                targets: vec![Target::Function { name: "debug_worker".into() }],
                filter: None,
            })
        );
    }

    #[test]
    fn where_point_predicates_both_orders() {
        let base = Select {
            from: Some(Table { name: "demo".into() }),
            targets: vec![Target::Column("payload".into())],
            filter: None,
        };
        let eq = |left, right| Select {
            filter: Some(Condition::Eq { left, right }),
            ..base.clone()
        };
        assert_eq!(
            one("SELECT payload FROM demo WHERE id = 42"),
            Stmt::Select(eq(
                Operand::Column("id".into()),
                Operand::Const(Value::Int(42))
            ))
        );
        // PG keeps written order — the wrapper mirrors it; callers normalize.
        assert_eq!(
            one("SELECT payload FROM demo WHERE 42 = id"),
            Stmt::Select(eq(
                Operand::Const(Value::Int(42)),
                Operand::Column("id".into())
            ))
        );
        assert_eq!(
            one("SELECT * FROM demo WHERE name = 'ada'"),
            Stmt::Select(Select {
                targets: vec![Target::Star],
                ..eq(
                    Operand::Column("name".into()),
                    Operand::Const(Value::Text("ada".into()))
                )
            })
        );
    }

    #[test]
    fn insert_with_escaped_quote() {
        assert_eq!(
            one("INSERT INTO debug_table VALUES (1, 'it''s')"),
            Stmt::Insert(Insert {
                table: "debug_table".into(),
                values: vec![Value::Int(1), Value::Text("it's".into())],
            })
        );
        // A column list does not disturb the positional mapping.
        assert_eq!(
            one("INSERT INTO debug_table (id, val) VALUES (2, 'x')"),
            Stmt::Insert(Insert {
                table: "debug_table".into(),
                values: vec![Value::Int(2), Value::Text("x".into())],
            })
        );
    }

    #[test]
    fn transaction_statements() {
        assert_eq!(one("BEGIN"), Stmt::Begin);
        assert_eq!(one("begin"), Stmt::Begin);
        assert_eq!(one("START TRANSACTION"), Stmt::Begin);
        assert_eq!(one("COMMIT"), Stmt::Commit);
        assert_eq!(one("END"), Stmt::Commit);
        assert_eq!(one("ROLLBACK"), Stmt::Rollback);
    }

    #[test]
    fn parsed_but_unmodeled_names_the_command() {
        assert_eq!(one("DROP TABLE users"), Stmt::Unsupported { command: "drop".into() });
        assert_eq!(one("UPDATE t SET x = 1"), Stmt::Unsupported { command: "update".into() });
        // A bare constant target parses; it is just an unmodeled *item*.
        assert_eq!(
            one("SELECT 1"),
            Stmt::Select(Select {
                targets: vec![Target::Unsupported],
                ..Select::default()
            })
        );
        assert_eq!(
            one("SELECT * FROM a UNION SELECT * FROM b"),
            Stmt::Unsupported { command: "select".into() }
        );
        assert_eq!(
            one("SELECT * FROM t GROUP BY x"),
            Stmt::Unsupported { command: "select".into() }
        );
        assert_eq!(
            one("SELECT * FROM t ORDER BY x"),
            Stmt::Unsupported { command: "select".into() }
        );
    }

    #[test]
    fn syntax_errors_carry_pg_text() {
        let err = parse("SELEC 1").expect_err("syntax error");
        assert!(err.message.contains("syntax error"), "{err:?}");
        assert!(err.message.contains("SELEC"), "{err:?}");
    }

    #[test]
    fn empty_and_comment_only() {
        let tree = parse("   ").expect("parses");
        assert!(tree.stmts().is_empty());
        let tree = parse("-- just a comment\n").expect("parses");
        assert!(tree.stmts().is_empty());
    }

    #[test]
    fn quoted_keyword_column_stays_a_column() {
        // `user` is a reserved word — quoted, it is still just a column ref.
        assert_eq!(
            one(r#"SELECT "user" FROM t"#),
            Stmt::Select(Select {
                from: Some(Table { name: "t".into() }),
                targets: vec![Target::Column("user".into())],
                filter: None,
            })
        );
    }

    #[test]
    fn where_columns_and_consts() {
        let stmt = one("SELECT * FROM t WHERE x = 5");
        let sel = select_of(&stmt);
        assert_eq!(
            sel.filter,
            Some(Condition::Eq {
                left: Operand::Column("x".into()),
                right: Operand::Const(Value::Int(5)),
            })
        );
        // PG's grammar folds unary minus into the constant itself.
        let neg = one("SELECT * FROM t WHERE id = -7");
        let sel = select_of(&neg);
        assert_eq!(
            sel.filter,
            Some(Condition::Eq {
                left: Operand::Column("id".into()),
                right: Operand::Const(Value::Int(-7)),
            })
        );
    }
}
