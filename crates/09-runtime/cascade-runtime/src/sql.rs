//! Micro-SQL parser for the debug engine (plan A1: the slice's fixed SQL
//! surface — one fixed-row table `debug_table(id INT, val TEXT)`,
//! `SELECT` projection, `INSERT`, `BEGIN`/`COMMIT`/`ROLLBACK`, and the
//! debug function `debug_worker()` used by tests to prove placement).
//!
//! This is deliberately not a real parser: `cascade-pg-parser` is an
//! unimplemented stub, and the slice needs exactly this subset. Errors carry
//! PG SQLSTATE codes so the session can emit real ErrorResponse messages.

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SqlError {
    #[error("syntax error in: {sql}")]
    Syntax { sql: String },
    #[error("undefined table: {name}")]
    UndefinedTable { name: String },
    #[error("column {name} does not exist")]
    UndefinedColumn { name: String },
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
    Select {
        cols: Vec<Col>,
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

/// Token stream: identifiers/keywords, integers, quoted strings, punctuation.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Word(String),
    Int(i64),
    Str(String),
    Comma,
    LParen,
    RParen,
    Semi,
    Star,
}

fn tokenize(sql: &str) -> Result<Vec<Tok>, SqlError> {
    let mut toks = Vec::new();
    let bytes: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            c if c.is_whitespace() => i += 1,
            ';' => {
                toks.push(Tok::Semi);
                i += 1;
            }
            ',' => {
                toks.push(Tok::Comma);
                i += 1;
            }
            '(' => {
                toks.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                toks.push(Tok::RParen);
                i += 1;
            }
            '*' => {
                toks.push(Tok::Star);
                i += 1;
            }
            '\'' => {
                // Single-quoted string with '' escape.
                let mut s = String::new();
                i += 1;
                loop {
                    if i >= bytes.len() {
                        return Err(SqlError::Syntax {
                            sql: sql.to_owned(),
                        });
                    }
                    if bytes[i] == '\'' {
                        if i + 1 < bytes.len() && bytes[i + 1] == '\'' {
                            s.push('\'');
                            i += 2;
                        } else {
                            i += 1;
                            break;
                        }
                    } else {
                        s.push(bytes[i]);
                        i += 1;
                    }
                }
                toks.push(Tok::Str(s));
            }
            c if c.is_ascii_digit() || c == '-' => {
                let start = i;
                if c == '-' {
                    i += 1;
                }
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                let text: String = bytes[start..i].iter().collect();
                let n: i64 = text.parse().map_err(|_| SqlError::Syntax {
                    sql: sql.to_owned(),
                })?;
                toks.push(Tok::Int(n));
            }
            c if c.is_alphabetic() || c == '_' => {
                let start = i;
                while i < bytes.len() && (bytes[i].is_alphanumeric() || bytes[i] == '_') {
                    i += 1;
                }
                toks.push(Tok::Word(bytes[start..i].iter().collect()));
            }
            _ => {
                return Err(SqlError::Syntax {
                    sql: sql.to_owned(),
                })
            }
        }
    }
    Ok(toks)
}

fn keyword(tok: Option<&Tok>, kw: &str) -> bool {
    matches!(tok, Some(Tok::Word(w)) if w.eq_ignore_ascii_case(kw))
}

/// Split a simple-query batch into statements on `;` (outside quotes).
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

fn peek(toks: &[Tok], pos: usize) -> Option<&Tok> {
    toks.get(pos)
}

fn next(toks: &[Tok], pos: &mut usize) -> Option<Tok> {
    let t = toks.get(*pos).cloned();
    *pos += 1;
    t
}

/// Parse one statement (no trailing `;` — callers strip it).
pub fn parse(sql: &str) -> Result<Stmt, SqlError> {
    let toks = tokenize(sql)?;
    let mut pos = 0;

    if toks.is_empty() {
        return Ok(Stmt::Empty);
    }

    let stmt = if keyword(peek(&toks, pos), "select") {
        pos += 1;
        // `*`, a column list, or the debug function call.
        if keyword(peek(&toks, pos), "debug_worker") {
            pos += 1;
            if !matches!(next(&toks, &mut pos), Some(Tok::LParen)) {
                return Err(SqlError::Syntax {
                    sql: sql.to_owned(),
                });
            }
            if !matches!(next(&toks, &mut pos), Some(Tok::RParen)) {
                return Err(SqlError::Syntax {
                    sql: sql.to_owned(),
                });
            }
            if pos != toks.len() {
                return Err(SqlError::Syntax {
                    sql: sql.to_owned(),
                });
            }
            Stmt::DebugWorker
        } else {
            let mut cols = Vec::new();
            if matches!(peek(&toks, pos), Some(Tok::Star)) {
                pos += 1;
                cols = vec![Col::Id, Col::Val];
            } else {
                loop {
                    match next(&toks, &mut pos) {
                        Some(Tok::Word(w)) if w.eq_ignore_ascii_case("id") => cols.push(Col::Id),
                        Some(Tok::Word(w)) if w.eq_ignore_ascii_case("val") => cols.push(Col::Val),
                        Some(Tok::Word(w)) => return Err(SqlError::UndefinedColumn { name: w }),
                        _ => {
                            return Err(SqlError::Syntax {
                                sql: sql.to_owned(),
                            })
                        }
                    }
                    if matches!(peek(&toks, pos), Some(Tok::Comma)) {
                        pos += 1;
                    } else {
                        break;
                    }
                }
            }
            if !keyword(peek(&toks, pos), "from") {
                return Err(SqlError::Syntax {
                    sql: sql.to_owned(),
                });
            }
            pos += 1;
            match next(&toks, &mut pos) {
                Some(Tok::Word(w)) if w.eq_ignore_ascii_case("debug_table") => {}
                Some(Tok::Word(w)) => return Err(SqlError::UndefinedTable { name: w }),
                _ => {
                    return Err(SqlError::Syntax {
                        sql: sql.to_owned(),
                    })
                }
            }
            if pos != toks.len() {
                return Err(SqlError::Syntax {
                    sql: sql.to_owned(),
                });
            }
            Stmt::Select { cols }
        }
    } else if keyword(peek(&toks, pos), "insert") {
        pos += 1;
        for kw in ["into", "debug_table", "values"] {
            if !keyword(peek(&toks, pos), kw) {
                return Err(if kw == "debug_table" {
                    // A different table name is a defined error, not syntax.
                    match peek(&toks, pos) {
                        Some(Tok::Word(w)) => SqlError::UndefinedTable { name: w.clone() },
                        _ => SqlError::Syntax {
                            sql: sql.to_owned(),
                        },
                    }
                } else {
                    SqlError::Syntax {
                        sql: sql.to_owned(),
                    }
                });
            }
            pos += 1;
        }
        if !matches!(next(&toks, &mut pos), Some(Tok::LParen)) {
            return Err(SqlError::Syntax {
                sql: sql.to_owned(),
            });
        }
        let id = match next(&toks, &mut pos) {
            Some(Tok::Int(n)) if n >= i32::MIN as i64 && n <= i32::MAX as i64 => n as i32,
            _ => {
                return Err(SqlError::Syntax {
                    sql: sql.to_owned(),
                })
            }
        };
        if !matches!(next(&toks, &mut pos), Some(Tok::Comma)) {
            return Err(SqlError::Syntax {
                sql: sql.to_owned(),
            });
        }
        let val = match next(&toks, &mut pos) {
            Some(Tok::Str(s)) => s,
            _ => {
                return Err(SqlError::Syntax {
                    sql: sql.to_owned(),
                })
            }
        };
        if !matches!(next(&toks, &mut pos), Some(Tok::RParen)) {
            return Err(SqlError::Syntax {
                sql: sql.to_owned(),
            });
        }
        if pos != toks.len() {
            return Err(SqlError::Syntax {
                sql: sql.to_owned(),
            });
        }
        Stmt::Insert { id, val }
    } else if keyword(peek(&toks, pos), "begin") {
        pos += 1;
        if pos != toks.len() {
            return Err(SqlError::Syntax {
                sql: sql.to_owned(),
            });
        }
        Stmt::Begin
    } else if keyword(peek(&toks, pos), "commit") {
        pos += 1;
        if pos != toks.len() {
            return Err(SqlError::Syntax {
                sql: sql.to_owned(),
            });
        }
        Stmt::Commit
    } else if keyword(peek(&toks, pos), "rollback") {
        pos += 1;
        if pos != toks.len() {
            return Err(SqlError::Syntax {
                sql: sql.to_owned(),
            });
        }
        Stmt::Rollback
    } else {
        return Err(SqlError::Syntax {
            sql: sql.to_owned(),
        });
    };

    Ok(stmt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_subset() {
        assert_eq!(
            parse("SELECT * FROM debug_table"),
            Ok(Stmt::Select {
                cols: vec![Col::Id, Col::Val]
            })
        );
        assert_eq!(
            parse("select id, val from debug_table"),
            Ok(Stmt::Select {
                cols: vec![Col::Id, Col::Val]
            })
        );
        assert_eq!(
            parse("SELECT val FROM debug_table"),
            Ok(Stmt::Select {
                cols: vec![Col::Val]
            })
        );
        assert_eq!(parse("SELECT debug_worker()"), Ok(Stmt::DebugWorker));
        assert_eq!(
            parse("INSERT INTO debug_table VALUES (1, 'hello')"),
            Ok(Stmt::Insert {
                id: 1,
                val: "hello".into()
            })
        );
        assert_eq!(
            parse("insert into debug_table values (-2, 'it''s')"),
            Ok(Stmt::Insert {
                id: -2,
                val: "it's".into()
            })
        );
        assert_eq!(parse("BEGIN"), Ok(Stmt::Begin));
        assert_eq!(parse("COMMIT"), Ok(Stmt::Commit));
        assert_eq!(parse("ROLLBACK"), Ok(Stmt::Rollback));
        assert_eq!(parse("   "), Ok(Stmt::Empty));
        // Trailing semicolon is stripped by the caller; single-statement
        // parse must not see it.
        assert_eq!(
            split_statements("SELECT 1; INSERT INTO debug_table VALUES (1, 'a;b')"),
            vec![
                "SELECT 1".to_owned(),
                " INSERT INTO debug_table VALUES (1, 'a;b')".to_owned()
            ]
        );
    }

    #[test]
    fn rejects_with_pg_error_codes() {
        assert_eq!(parse("SELECT * FROM other").unwrap_err().code(), "42P01");
        assert_eq!(
            parse("SELECT nope FROM debug_table").unwrap_err().code(),
            "42703"
        );
        assert_eq!(parse("DROP TABLE users").unwrap_err().code(), "42601");
        assert_eq!(
            parse("INSERT INTO other VALUES (1, 'x')")
                .unwrap_err()
                .code(),
            "42P01"
        );
        assert_eq!(
            parse("SELECT * FROM debug_table WHERE x=1")
                .unwrap_err()
                .code(),
            "42601",
            "WHERE is not in the A1 subset"
        );
        assert_eq!(
            parse("UPDATE debug_table SET id = 2").unwrap_err().code(),
            "42601"
        );
    }
}
