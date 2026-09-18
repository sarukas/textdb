//! Front-matter properties: how they flatten for indexing, and the query language that asks
//! about them.
//!
//! Both bindings keep the same shape — one row per (document, property path, value) — so one
//! query text means the same thing on SQLite and on Postgres. This module owns the two halves
//! neither binding should have its own version of: turning parsed front matter into those
//! rows, and turning a query string into a tree the bindings compile to SQL.

use serde_json::Value;

/// One indexable property value.
///
/// A list becomes one `Prop` per element, keeping its position in `ord`, so "does `tags`
/// contain `telco`" is an ordinary equality against a row rather than a scan inside a JSON
/// array. A nested object flattens to dotted paths (`project.name`), which is what people
/// already type in Obsidian and in `json_extract`.
#[derive(Clone, Debug, PartialEq)]
pub struct Prop {
    pub key: String,
    /// The value as text, for everything that is not a bare number.
    pub text: Option<String>,
    /// The value as a number, when it is one, so `priority > 3` compares numerically rather
    /// than lexically (where "10" sorts before "9").
    pub num: Option<f64>,
    /// Position within a list; 0 for a scalar.
    pub ord: i64,
}

/// Flatten front matter into the rows that get indexed.
///
/// Scalars keep their type: a bool is stored as `true`/`false` text so it can be searched by
/// name, a number gets both a text and a numeric form so `year:2026` and `year:>2000` both
/// work. `null` is recorded with neither, which still lets `has:key` find it — a property
/// present but empty is a real state in a vault and hiding it would be wrong.
pub fn flatten(data: &Value) -> Vec<Prop> {
    let mut out = Vec::new();
    if let Value::Object(map) = data {
        for (k, v) in map {
            walk(k, v, 0, &mut out);
        }
    }
    out
}

fn walk(key: &str, v: &Value, ord: i64, out: &mut Vec<Prop>) {
    match v {
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk(key, item, i as i64, out);
            }
        }
        Value::Object(map) => {
            for (k, inner) in map {
                walk(&format!("{}.{}", key, k), inner, ord, out);
            }
        }
        Value::String(s) => out.push(Prop {
            key: key.to_string(),
            text: Some(s.clone()),
            num: None,
            ord,
        }),
        Value::Number(n) => out.push(Prop {
            key: key.to_string(),
            text: Some(n.to_string()),
            num: n.as_f64(),
            ord,
        }),
        Value::Bool(b) => out.push(Prop {
            key: key.to_string(),
            text: Some(b.to_string()),
            num: None,
            ord,
        }),
        Value::Null => out.push(Prop {
            key: key.to_string(),
            text: None,
            num: None,
            ord,
        }),
    }
}

// ---------------------------------------------------------------------------------------
// The query language
// ---------------------------------------------------------------------------------------

/// How a term compares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// `key:value` — equal, case-insensitively, which is how people expect a vault to behave.
    Eq,
    /// `key:!=value`
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// `key:*` or `has:key` — the property is present, whatever it holds.
    Exists,
    /// `key:val*` — the value starts with this.
    Prefix,
    /// `key:~text` — the value contains this.
    Contains,
}

/// One comparison against one property.
#[derive(Clone, Debug, PartialEq)]
pub struct Term {
    pub key: String,
    pub op: Op,
    pub value: String,
    /// `value` parsed as a number, when it is one.
    pub num: Option<f64>,
}

/// A parsed query.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    Term(Term),
    And(Vec<Expr>),
    Or(Vec<Expr>),
    Not(Box<Expr>),
    /// Matches every document that has any front matter — what an empty query means.
    All,
}

#[derive(Debug, PartialEq, Eq)]
pub struct QueryError {
    pub message: String,
    /// Byte offset in the query where it went wrong, for an editor to point at.
    pub at: usize,
}

impl std::fmt::Display for QueryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} (at {})", self.message, self.at)
    }
}

impl std::error::Error for QueryError {}

/// Parse a query.
///
/// Deliberately the shape people already type in Obsidian rather than something new:
///
/// ```text
/// status:draft                     a property equals a value
/// tags:telco                       a list contains a value — lists are rows, so this is Eq
/// project.name:atlas               nested properties are dotted
/// title:"quarterly review"         quote a value with spaces
/// priority:>3   due:<=2026-10-01   comparisons; numeric when both sides are numbers
/// has:budget    budget:*           the property exists at all
/// name:proj*    notes:~telco       starts with, contains
/// status:draft tags:telco          space means AND
/// status:draft OR status:review    explicit OR
/// -status:archived                 NOT, either as `-` or as the word
/// (a OR b) AND NOT c               grouping
/// ```
///
/// An empty query is [`Expr::All`] rather than an error: the UI asks with an empty box first,
/// and "everything" is the honest answer to "no filter".
pub fn parse(input: &str) -> Result<Expr, QueryError> {
    let mut p = Parser {
        toks: lex(input)?,
        at: 0,
    };
    if p.toks.is_empty() {
        return Ok(Expr::All);
    }
    let e = p.or()?;
    if let Some(t) = p.peek() {
        return Err(QueryError {
            message: format!("unexpected {}", t.describe()),
            at: t.at(),
        });
    }
    Ok(e)
}

#[derive(Clone, Debug, PartialEq)]
enum Tok {
    Word { s: String, quoted: bool, at: usize },
    And(usize),
    Or(usize),
    Not(usize),
    Open(usize),
    Close(usize),
}

impl Tok {
    fn at(&self) -> usize {
        match self {
            Tok::Word { at, .. } | Tok::And(at) | Tok::Or(at) | Tok::Not(at) | Tok::Open(at) | Tok::Close(at) => *at,
        }
    }
    fn describe(&self) -> String {
        match self {
            Tok::Word { s, .. } => format!("`{}`", s),
            Tok::And(_) => "AND".into(),
            Tok::Or(_) => "OR".into(),
            Tok::Not(_) => "NOT".into(),
            Tok::Open(_) => "(".into(),
            Tok::Close(_) => ")".into(),
        }
    }
}

/// A token with the offset it started at, for error reporting.
struct Located {
    tok: Tok,
    at: usize,
}

fn lex(input: &str) -> Result<Vec<Located>, QueryError> {
    let b = input.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        match c {
            b'(' => {
                out.push(Located { tok: Tok::Open(start), at: start });
                i += 1;
            }
            b')' => {
                out.push(Located { tok: Tok::Close(start), at: start });
                i += 1;
            }
            b'-' if i + 1 < b.len() && !b[i + 1].is_ascii_whitespace() => {
                // Obsidian's `-term`. A bare `-` is just text (a value may legitimately be one).
                out.push(Located { tok: Tok::Not(start), at: start });
                i += 1;
            }
            _ => {
                // A word runs to whitespace or a bracket, except inside quotes, and a quoted
                // run may sit in the middle of one so `key:"two words"` is a single word.
                let mut s = String::new();
                let mut quoted = false;
                while i < b.len() {
                    match b[i] {
                        b'"' => {
                            quoted = true;
                            i += 1;
                            let from = i;
                            while i < b.len() && b[i] != b'"' {
                                i += 1;
                            }
                            if i >= b.len() {
                                return Err(QueryError {
                                    message: "unclosed quote".into(),
                                    at: from.saturating_sub(1),
                                });
                            }
                            s.push_str(&input[from..i]);
                            i += 1;
                        }
                        c if c.is_ascii_whitespace() || c == b'(' || c == b')' => break,
                        _ => {
                            let from = i;
                            while i < b.len() && !input.is_char_boundary(i) {
                                i += 1;
                            }
                            i += 1;
                            while i < b.len() && !input.is_char_boundary(i) {
                                i += 1;
                            }
                            s.push_str(&input[from..i]);
                        }
                    }
                }
                let upper = s.to_ascii_uppercase();
                let tok = match upper.as_str() {
                    "AND" if !quoted => Tok::And(start),
                    "OR" if !quoted => Tok::Or(start),
                    "NOT" if !quoted => Tok::Not(start),
                    _ => Tok::Word { s, quoted, at: start },
                };
                out.push(Located { tok, at: start });
            }
        }
    }
    Ok(out)
}

struct Parser {
    toks: Vec<Located>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.at).map(|l| &l.tok)
    }
    fn bump(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.at).map(|l| l.tok.clone());
        if t.is_some() {
            self.at += 1;
        }
        t
    }
    fn end(&self) -> usize {
        self.toks.last().map(|l| l.at).unwrap_or(0)
    }

    fn or(&mut self) -> Result<Expr, QueryError> {
        let mut parts = vec![self.and()?];
        while matches!(self.peek(), Some(Tok::Or(_))) {
            self.bump();
            parts.push(self.and()?);
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Expr::Or(parts) })
    }

    /// `AND` is also implicit: two terms side by side mean both, as they do in Obsidian.
    fn and(&mut self) -> Result<Expr, QueryError> {
        let mut parts = vec![self.unary()?];
        loop {
            match self.peek() {
                Some(Tok::And(_)) => {
                    self.bump();
                    parts.push(self.unary()?);
                }
                Some(Tok::Word { .. }) | Some(Tok::Not(_)) | Some(Tok::Open(_)) => parts.push(self.unary()?),
                _ => break,
            }
        }
        Ok(if parts.len() == 1 { parts.pop().unwrap() } else { Expr::And(parts) })
    }

    fn unary(&mut self) -> Result<Expr, QueryError> {
        if matches!(self.peek(), Some(Tok::Not(_))) {
            self.bump();
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Expr, QueryError> {
        match self.bump() {
            Some(Tok::Open(at)) => {
                let e = self.or()?;
                match self.bump() {
                    Some(Tok::Close(_)) => Ok(e),
                    _ => Err(QueryError {
                        message: "unclosed (".into(),
                        at,
                    }),
                }
            }
            Some(Tok::Word { s, quoted, at }) => term(&s, quoted, at).map(Expr::Term),
            Some(t) => Err(QueryError {
                message: format!("expected a term, found {}", t.describe()),
                at: t.at(),
            }),
            None => Err(QueryError {
                message: "the query ends early".into(),
                at: self.end(),
            }),
        }
    }
}

fn term(word: &str, quoted: bool, at: usize) -> Result<Term, QueryError> {
    // `has:key` reads better than `key:*` and Obsidian users type both.
    if let Some(rest) = word.strip_prefix("has:").or_else(|| word.strip_prefix("HAS:")) {
        if rest.is_empty() {
            return Err(QueryError {
                message: "has: needs a property name".into(),
                at,
            });
        }
        return Ok(Term {
            key: rest.to_string(),
            op: Op::Exists,
            value: String::new(),
            num: None,
        });
    }
    let Some((key, raw)) = word.split_once(':') else {
        return Err(QueryError {
            message: format!("`{}` is not a property filter — write key:value, or has:key", word),
            at,
        });
    };
    if key.is_empty() {
        return Err(QueryError {
            message: "a filter needs a property name before the colon".into(),
            at,
        });
    }
    // A quoted value is taken literally: `status:">3"` looks for the text, not a comparison.
    let (op, value) = if quoted {
        (Op::Eq, raw.to_string())
    } else if raw == "*" {
        (Op::Exists, String::new())
    } else if let Some(v) = raw.strip_prefix(">=") {
        (Op::Ge, v.to_string())
    } else if let Some(v) = raw.strip_prefix("<=") {
        (Op::Le, v.to_string())
    } else if let Some(v) = raw.strip_prefix("!=") {
        (Op::Ne, v.to_string())
    } else if let Some(v) = raw.strip_prefix('>') {
        (Op::Gt, v.to_string())
    } else if let Some(v) = raw.strip_prefix('<') {
        (Op::Lt, v.to_string())
    } else if let Some(v) = raw.strip_prefix('~') {
        (Op::Contains, v.to_string())
    } else if let Some(v) = raw.strip_suffix('*') {
        (Op::Prefix, v.to_string())
    } else {
        (Op::Eq, raw.to_string())
    };
    if value.is_empty() && !matches!(op, Op::Exists) {
        return Err(QueryError {
            message: format!("`{}` has no value — write {}:something, or {}:* for any", word, key, key),
            at,
        });
    }
    Ok(Term {
        num: value.parse::<f64>().ok(),
        key: key.to_string(),
        op,
        value,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(key: &str, op: Op, value: &str) -> Expr {
        Expr::Term(Term {
            key: key.into(),
            op,
            value: value.into(),
            num: value.parse().ok(),
        })
    }

    #[test]
    fn a_list_becomes_one_row_per_element_and_keeps_its_order() {
        let v: Value = serde_json::from_str(r#"{"tags":["cvm","telco"]}"#).unwrap();
        let got = flatten(&v);
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].key.as_str(), got[0].text.as_deref(), got[0].ord), ("tags", Some("cvm"), 0));
        assert_eq!((got[1].key.as_str(), got[1].text.as_deref(), got[1].ord), ("tags", Some("telco"), 1));
    }

    #[test]
    fn nested_objects_flatten_to_dotted_paths() {
        let v: Value = serde_json::from_str(r#"{"project":{"name":"atlas","phase":"pilot"}}"#).unwrap();
        let mut keys: Vec<String> = flatten(&v).into_iter().map(|p| p.key).collect();
        keys.sort();
        assert_eq!(keys, vec!["project.name", "project.phase"]);
    }

    #[test]
    fn numbers_are_stored_as_both_text_and_number() {
        let v: Value = serde_json::from_str(r#"{"priority":3}"#).unwrap();
        let p = &flatten(&v)[0];
        assert_eq!(p.text.as_deref(), Some("3"));
        assert_eq!(p.num, Some(3.0));
    }

    #[test]
    fn a_null_property_is_still_recorded_so_has_can_find_it() {
        let v: Value = serde_json::from_str(r#"{"due":null}"#).unwrap();
        let p = &flatten(&v)[0];
        assert_eq!((p.text.as_deref(), p.num), (None, None));
    }

    #[test]
    fn a_bare_query_is_an_equality() {
        assert_eq!(parse("status:draft").unwrap(), t("status", Op::Eq, "draft"));
    }

    #[test]
    fn spaces_mean_and() {
        assert_eq!(
            parse("status:draft tags:telco").unwrap(),
            Expr::And(vec![t("status", Op::Eq, "draft"), t("tags", Op::Eq, "telco")])
        );
    }

    #[test]
    fn or_binds_looser_than_and() {
        let got = parse("a:1 b:2 OR c:3").unwrap();
        assert_eq!(got, Expr::Or(vec![Expr::And(vec![t("a", Op::Eq, "1"), t("b", Op::Eq, "2")]), t("c", Op::Eq, "3")]));
    }

    #[test]
    fn parentheses_regroup() {
        let got = parse("a:1 AND (b:2 OR c:3)").unwrap();
        assert_eq!(got, Expr::And(vec![t("a", Op::Eq, "1"), Expr::Or(vec![t("b", Op::Eq, "2"), t("c", Op::Eq, "3")])]));
    }

    #[test]
    fn both_spellings_of_not() {
        let want = Expr::Not(Box::new(t("status", Op::Eq, "archived")));
        assert_eq!(parse("-status:archived").unwrap(), want);
        assert_eq!(parse("NOT status:archived").unwrap(), want);
    }

    #[test]
    fn comparisons_and_existence() {
        assert_eq!(parse("priority:>3").unwrap(), t("priority", Op::Gt, "3"));
        assert_eq!(parse("priority:>=3").unwrap(), t("priority", Op::Ge, "3"));
        assert_eq!(parse("priority:!=3").unwrap(), t("priority", Op::Ne, "3"));
        assert_eq!(parse("has:budget").unwrap(), t("budget", Op::Exists, ""));
        assert_eq!(parse("budget:*").unwrap(), t("budget", Op::Exists, ""));
    }

    #[test]
    fn prefix_and_contains() {
        assert_eq!(parse("name:atl*").unwrap(), t("name", Op::Prefix, "atl"));
        assert_eq!(parse("note:~telco").unwrap(), t("note", Op::Contains, "telco"));
    }

    #[test]
    fn a_quoted_value_keeps_its_spaces_and_is_never_an_operator() {
        assert_eq!(parse(r#"title:"quarterly review""#).unwrap(), t("title", Op::Eq, "quarterly review"));
        // Quoted, so `>3` is the text to match and not a comparison.
        assert_eq!(parse(r#"label:">3""#).unwrap(), t("label", Op::Eq, ">3"));
    }

    #[test]
    fn and_or_not_are_keywords_only_unquoted() {
        assert_eq!(parse(r#"tag:"and""#).unwrap(), t("tag", Op::Eq, "and"));
    }

    #[test]
    fn an_empty_query_means_everything() {
        assert_eq!(parse("").unwrap(), Expr::All);
        assert_eq!(parse("   ").unwrap(), Expr::All);
    }

    #[test]
    fn errors_point_at_the_offending_offset() {
        let e = parse("status:draft AND").unwrap_err();
        assert!(e.message.contains("ends early"), "{}", e.message);
        let e = parse("draft").unwrap_err();
        assert!(e.message.contains("not a property filter"), "{}", e.message);
        let e = parse("a:1 (b:2").unwrap_err();
        assert!(e.message.contains("unclosed ("), "{}", e.message);
        let e = parse(r#"a:"b"#).unwrap_err();
        assert!(e.message.contains("unclosed quote"), "{}", e.message);
        let e = parse("status:").unwrap_err();
        assert!(e.message.contains("no value"), "{}", e.message);
    }

    #[test]
    fn nested_keys_and_unicode_values_survive() {
        assert_eq!(parse("project.name:atlas").unwrap(), t("project.name", Op::Eq, "atlas"));
        assert_eq!(parse("autorius:Šarūnas").unwrap(), t("autorius", Op::Eq, "Šarūnas"));
    }
}
