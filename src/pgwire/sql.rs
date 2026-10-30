//! Text-level handling of SQL that clients send before Quarry's parser sees
//! it: splitting a query string into statements, recognizing transaction and
//! session commands, and substituting `$1`-style parameters.

/// What each byte of a SQL string is part of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Code,
    /// Inside a '...' string or a "..." identifier.
    Quoted,
    Comment,
}

/// Classifies every byte, so callers only act on `;` and `$` in code.
fn classify(sql: &str) -> Vec<Kind> {
    let b = sql.as_bytes();
    let mut kinds = vec![Kind::Code; b.len()];
    let mut i = 0;
    while i < b.len() {
        let start = i;
        let kind = match b[i] {
            q @ (b'\'' | b'"') => {
                // Ends at the next unpaired quote; '' inside is an escape.
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                Kind::Quoted
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                Kind::Comment
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i < b.len() && !(b[i] == b'*' && b.get(i + 1) == Some(&b'/')) {
                    i += 1;
                }
                i += 1;
                Kind::Comment
            }
            _ => Kind::Code,
        };
        let end = (i + 1).min(b.len());
        kinds[start..end].fill(kind);
        i = end;
    }
    kinds
}

/// Splits a query string on semicolons outside strings and comments,
/// dropping statements that are empty or only comments. Each statement comes
/// with its byte offset in `sql`.
pub fn split_statements(sql: &str) -> Vec<(usize, &str)> {
    let kinds = classify(sql);
    let mut out = Vec::new();
    let mut start = 0;
    for i in 0..=sql.len() {
        if i == sql.len() || (sql.as_bytes()[i] == b';' && kinds[i] == Kind::Code) {
            let blank = (start..i)
                .all(|j| kinds[j] == Kind::Comment || sql.as_bytes()[j].is_ascii_whitespace());
            if !blank {
                out.push((start, &sql[start..i]));
            }
            start = i + 1;
        }
    }
    out
}

/// Commands that Quarry accepts so clients work, without doing anything:
/// it has no transactions or session settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Begin,
    Commit,
    Rollback,
    /// SET, RESET, DISCARD, DEALLOCATE: acknowledged and ignored. The string
    /// is the command tag to reply with.
    Ignored(&'static str),
}

pub fn control_command(stmt: &str) -> Option<Control> {
    let word: String = strip_comments(stmt)
        .trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase();
    Some(match word.as_str() {
        "BEGIN" | "START" => Control::Begin,
        "COMMIT" | "END" => Control::Commit,
        "ROLLBACK" | "ABORT" => Control::Rollback,
        "SET" => Control::Ignored("SET"),
        "RESET" => Control::Ignored("RESET"),
        "DISCARD" => Control::Ignored("DISCARD ALL"),
        "DEALLOCATE" => Control::Ignored("DEALLOCATE"),
        _ => return None,
    })
}

/// The statement with comments replaced by spaces.
fn strip_comments(sql: &str) -> String {
    let kinds = classify(sql);
    sql.char_indices()
        .map(|(i, c)| if kinds[i] == Kind::Comment { ' ' } else { c })
        .collect()
}

/// One bound parameter: its type OID from the client (0 if unspecified) and
/// its text value, or `None` for NULL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Param {
    pub type_oid: i32,
    pub value: Option<String>,
}

/// Replaces `$1`, `$2`, ... outside strings and comments with SQL literals,
/// since Quarry's parser has no placeholders. Values are quoted as strings
/// unless the client declared a numeric, boolean, or date type.
pub fn substitute_params(sql: &str, params: &[Param]) -> Result<String, String> {
    let kinds = classify(sql);
    let b = sql.as_bytes();
    let mut out = String::with_capacity(sql.len());
    let mut i = 0;
    let mut copied = 0;
    while i < b.len() {
        if b[i] == b'$' && kinds[i] == Kind::Code {
            let digits = b[i + 1..].iter().take_while(|c| c.is_ascii_digit()).count();
            if digits > 0 {
                let n: usize = sql[i + 1..i + 1 + digits]
                    .parse()
                    .map_err(|_| format!("bad parameter ${}", &sql[i + 1..i + 1 + digits]))?;
                let param = n
                    .checked_sub(1)
                    .and_then(|k| params.get(k))
                    .ok_or_else(|| format!("there is no parameter ${n}"))?;
                out.push_str(&sql[copied..i]);
                out.push_str(&literal(param)?);
                i += 1 + digits;
                copied = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&sql[copied..]);
    Ok(out)
}

/// The number of parameters a statement uses: its highest `$n` outside
/// strings and comments.
pub fn param_count(sql: &str) -> usize {
    let kinds = classify(sql);
    let b = sql.as_bytes();
    let mut max = 0;
    for i in 0..b.len() {
        if b[i] == b'$' && kinds[i] == Kind::Code {
            let digits: String = sql[i + 1..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            max = max.max(digits.parse().unwrap_or(0));
        }
    }
    max
}

// Type OIDs from Postgres's pg_type catalog.
pub const BOOL: i32 = 16;
pub const INT8: i32 = 20;
pub const INT2: i32 = 21;
pub const INT4: i32 = 23;
pub const TEXT: i32 = 25;
pub const VARCHAR: i32 = 1043;
pub const FLOAT4: i32 = 700;
pub const FLOAT8: i32 = 701;
pub const DATE: i32 = 1082;
pub const INTERVAL: i32 = 1186;
pub const NUMERIC: i32 = 1700;

fn literal(p: &Param) -> Result<String, String> {
    let Some(v) = &p.value else {
        return Ok("NULL".into());
    };
    let quoted = || format!("'{}'", v.replace('\'', "''"));
    Ok(match p.type_oid {
        INT2 | INT4 | INT8 | FLOAT4 | FLOAT8 | NUMERIC => {
            if v.parse::<f64>().is_err() {
                return Err(format!("invalid number {v:?} for a numeric parameter"));
            }
            // A negative value is parenthesized so `1-$1` can't become a
            // `--` comment.
            if v.trim_start().starts_with('-') {
                format!("({v})")
            } else {
                v.clone()
            }
        }
        BOOL => match v.to_ascii_lowercase().as_str() {
            "t" | "true" | "1" | "y" | "yes" | "on" => "TRUE".into(),
            "f" | "false" | "0" | "n" | "no" | "off" => "FALSE".into(),
            _ => return Err(format!("invalid boolean {v:?}")),
        },
        DATE => format!("DATE {}", quoted()),
        _ => quoted(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_outside_strings_and_comments() {
        let sql = "select ';' as a; -- one; two\nselect 2 /* ; */;  ;\n-- only a comment";
        let parts: Vec<&str> = split_statements(sql).into_iter().map(|(_, s)| s).collect();
        assert_eq!(parts, ["select ';' as a", " -- one; two\nselect 2 /* ; */"]);
        assert!(split_statements("  ").is_empty());
        assert_eq!(split_statements("select 1;select 2")[1], (9, "select 2"));
    }

    #[test]
    fn recognizes_control_commands() {
        assert_eq!(control_command("  begin"), Some(Control::Begin));
        assert_eq!(control_command("START TRANSACTION"), Some(Control::Begin));
        assert_eq!(control_command("/* x */ commit"), Some(Control::Commit));
        assert_eq!(
            control_command("SET application_name = 'x'"),
            Some(Control::Ignored("SET"))
        );
        assert_eq!(control_command("select 1"), None);
        assert_eq!(control_command("settings"), None);
    }

    #[test]
    fn substitutes_parameters_as_literals() {
        let p = |type_oid, v: Option<&str>| Param {
            type_oid,
            value: v.map(str::to_string),
        };
        let params = [
            p(INT8, Some("-5")),
            p(0, Some("it's")),
            p(BOOL, Some("t")),
            p(DATE, Some("2024-01-31")),
            p(TEXT, None),
            p(INT4, Some("7")),
        ];
        let sql = "select $1, $2, $3, $4, $5, '$1' -- $2\n, 1-$1, $6";
        assert_eq!(
            substitute_params(sql, &params).unwrap(),
            "select (-5), 'it''s', TRUE, DATE '2024-01-31', NULL, '$1' -- $2\n, 1-(-5), 7"
        );
        assert!(substitute_params("select $7", &params).is_err());
        assert_eq!(param_count(sql), 6);
        assert_eq!(param_count("select '$9'"), 0);
        assert!(substitute_params("select $1", &[p(INT4, Some("1; drop"))]).is_err());
    }
}
