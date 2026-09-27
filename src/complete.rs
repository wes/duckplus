//! Schema-aware SQL autocomplete.
//!
//! A small tokenizer reads the statement around the cursor and works out
//! what belongs there: a relation after FROM/JOIN, a column of an in-scope
//! table after `alias.`, a column, function or keyword inside an
//! expression. Candidates come from the loaded catalog (plus CTEs and
//! subqueries in the statement itself) and are ranked by how well they
//! match and how close they are. Pure and synchronous, so it runs on every
//! keystroke.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::rc::Rc;

use gpui_kit::component::input::{CompletionProvider, Rope, RopeExt as _};
use gpui_kit::{App, Task, Window};
use lsp_types::{
    CompletionContext, CompletionItem, CompletionItemKind, CompletionResponse, CompletionTextEdit,
    TextEdit,
};

use crate::quack::Catalog;

/// At most this many suggestions are shown.
const MAX_ITEMS: usize = 50;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Column,
    Alias,
    Relation,
    Schema,
    Database,
    Function,
    Keyword,
    Type,
}

#[derive(Debug, Clone)]
pub struct Suggestion {
    pub label: String,
    pub insert: String,
    pub detail: Option<String>,
    pub kind: Kind,
    /// Byte range of the buffer the insert replaces.
    pub range: Range<usize>,
    /// Leading bytes of `label` that matched what was typed.
    pub matched: usize,
}

// ── schema index ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
struct Col {
    name: String,
    ty: String,
}

#[derive(Debug)]
struct Rel {
    db: String,
    schema: String,
    name: String,
    is_view: bool,
    cols: Vec<Col>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum FnKind {
    Scalar,
    Aggregate,
    Table,
}

/// Everything autocomplete knows about the server, built once per catalog
/// load (or database focus change).
#[derive(Debug, Default)]
pub struct SchemaIndex {
    rels: Vec<Rel>,
    databases: Vec<String>,
    functions: Vec<(String, FnKind)>,
    reserved: HashSet<String>,
    /// Where unqualified names resolve.
    default_db: String,
    default_schema: String,
}

impl SchemaIndex {
    pub fn new(catalog: &Catalog, focus_db: Option<&str>) -> Self {
        let mut cols: HashMap<(&str, &str, &str), Vec<Col>> = HashMap::new();
        for c in &catalog.columns {
            cols.entry((&c.database, &c.schema, &c.table))
                .or_default()
                .push(Col {
                    name: c.name.clone(),
                    ty: c.data_type.clone(),
                });
        }
        let rels = catalog
            .relations
            .iter()
            .map(|r| Rel {
                db: r.database.clone(),
                schema: r.schema.clone(),
                name: r.name.clone(),
                is_view: r.is_view,
                cols: cols
                    .remove(&(r.database.as_str(), r.schema.as_str(), r.name.as_str()))
                    .unwrap_or_default(),
            })
            .collect();

        // A name can be both a scalar function and a macro, say; keep the
        // most specific kind.
        let mut functions: HashMap<&str, FnKind> = HashMap::new();
        for (name, ty) in &catalog.functions {
            if !name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
                continue; // operators like `+` or `~~`
            }
            let kind = match ty.as_str() {
                "table" | "table_macro" => FnKind::Table,
                "aggregate" => FnKind::Aggregate,
                _ => FnKind::Scalar,
            };
            let e = functions.entry(name).or_insert(kind);
            *e = (*e).max(kind);
        }
        let mut functions: Vec<(String, FnKind)> = functions
            .into_iter()
            .map(|(n, k)| (n.to_string(), k))
            .collect();
        functions.sort();

        let (default_db, default_schema) = match focus_db {
            Some(db) => (db.to_string(), "main".to_string()),
            None => (
                catalog.current_database.clone().unwrap_or_default(),
                catalog
                    .current_schema
                    .clone()
                    .unwrap_or_else(|| "main".into()),
            ),
        };
        Self {
            rels,
            databases: catalog.databases.clone(),
            functions,
            reserved: catalog.reserved.iter().map(|k| k.to_lowercase()).collect(),
            default_db,
            default_schema,
        }
    }

    /// The relation a written name refers to, resolved the way DuckDB does,
    /// then leniently by bare name so half-written queries still get columns.
    fn resolve(&self, parts: &[String]) -> Option<&Rel> {
        let find = |d: &str, s: &str, n: &str| {
            self.rels
                .iter()
                .find(|r| eq(&r.db, d) && eq(&r.schema, s) && eq(&r.name, n))
        };
        match parts {
            [n] => find(&self.default_db, &self.default_schema, n)
                .or_else(|| {
                    self.rels
                        .iter()
                        .find(|r| eq(&r.db, &self.default_db) && eq(&r.name, n))
                })
                .or_else(|| self.rels.iter().find(|r| eq(&r.name, n))),
            [a, n] => find(&self.default_db, a, n).or_else(|| find(a, "main", n)),
            [d, s, n] => find(d, s, n),
            _ => None,
        }
    }

    fn is_database(&self, name: &str) -> bool {
        self.databases.iter().any(|d| eq(d, name))
    }

    fn is_schema(&self, db: &str, name: &str) -> bool {
        self.rels
            .iter()
            .any(|r| eq(&r.db, db) && eq(&r.schema, name))
    }

    /// Whether `name` must be written as a quoted identifier.
    fn needs_quote(&self, name: &str) -> bool {
        let mut chars = name.chars();
        let plain = chars
            .next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || c == '_');
        !plain || self.reserved.contains(&name.to_ascii_lowercase())
    }

    fn ident(&self, name: &str, force_quote: bool) -> String {
        if force_quote || self.needs_quote(name) {
            format!("\"{}\"", name.replace('"', "\"\""))
        } else {
            name.to_string()
        }
    }
}

fn eq(a: &str, b: &str) -> bool {
    a.eq_ignore_ascii_case(b)
}

// ── tokenizer ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum T {
    Word,
    /// `"quoted identifier"`; `closed` is false when it runs to the end.
    Quoted {
        closed: bool,
    },
    Str,
    Num,
    Punct(u8),
}

#[derive(Debug, Clone, Copy)]
struct Tok {
    t: T,
    start: usize,
    end: usize,
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || !c.is_ascii()
}

/// Tokens of `src`, or `None` if `cursor` sits inside a comment or string,
/// where nothing should be suggested.
fn lex(src: &str, cursor: usize) -> Option<Vec<Tok>> {
    let b = src.as_bytes();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if c == b'-' && b.get(i + 1) == Some(&b'-') {
            i = src[i..].find('\n').map_or(b.len(), |n| i + n);
            if cursor > start + 1 && cursor <= i {
                return None;
            }
            continue;
        }
        if c == b'/' && b.get(i + 1) == Some(&b'*') {
            i = src[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2);
            if cursor > start + 1 && (cursor < i || i == b.len()) {
                return None;
            }
            continue;
        }
        if c == b'\'' || c == b'"' {
            i += 1;
            let mut closed = false;
            while i < b.len() {
                if b[i] == c {
                    if b.get(i + 1) == Some(&c) {
                        i += 2;
                        continue;
                    }
                    i += 1;
                    closed = true;
                    break;
                }
                i += 1;
            }
            if c == b'\'' {
                if cursor > start && (cursor < i || !closed) {
                    return None;
                }
                toks.push(Tok {
                    t: T::Str,
                    start,
                    end: i,
                });
            } else {
                toks.push(Tok {
                    t: T::Quoted { closed },
                    start,
                    end: i,
                });
            }
            continue;
        }
        let ch = src[i..].chars().next().unwrap();
        if ch.is_ascii_digit() {
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'.' || b[i] == b'_') {
                i += 1;
            }
            toks.push(Tok {
                t: T::Num,
                start,
                end: i,
            });
            continue;
        }
        if is_ident_char(ch) {
            for c in src[i..].chars() {
                if !is_ident_char(c) {
                    break;
                }
                i += c.len_utf8();
            }
            toks.push(Tok {
                t: T::Word,
                start,
                end: i,
            });
            continue;
        }
        i += ch.len_utf8();
        toks.push(Tok {
            t: T::Punct(c),
            start,
            end: i,
        });
    }
    Some(toks)
}

// ── statement analysis ───────────────────────────────────────────────────

const REL_KWS: &[&str] = &[
    "from",
    "join",
    "update",
    "into",
    "table",
    "describe",
    "summarize",
];

/// Keywords that start the clause the cursor is in.
const CLAUSE_KWS: &[&str] = &[
    "select",
    "from",
    "join",
    "where",
    "on",
    "using",
    "having",
    "by",
    "set",
    "qualify",
    "update",
    "into",
    "table",
    "describe",
    "summarize",
    "values",
    "returning",
    "when",
    "then",
    "else",
    "and",
    "or",
    "not",
    "case",
    "limit",
    "offset",
];

/// Words that can follow a relation but are never its alias.
const STOP_WORDS: &[&str] = &[
    "where",
    "join",
    "on",
    "using",
    "left",
    "right",
    "inner",
    "outer",
    "full",
    "cross",
    "natural",
    "group",
    "order",
    "limit",
    "offset",
    "select",
    "set",
    "values",
    "union",
    "except",
    "intersect",
    "having",
    "window",
    "qualify",
    "positional",
    "asof",
    "anti",
    "semi",
    "lateral",
    "sample",
    "tablesample",
    "returning",
    "by",
    "pivot",
    "unpivot",
    "default",
    "from",
    "into",
    "with",
    "and",
    "or",
    "not",
    "as",
    "then",
    "when",
    "else",
    "end",
    "by",
    "to",
];

const START_KWS: &[&str] = &[
    "SELECT",
    "FROM",
    "WITH",
    "INSERT INTO",
    "UPDATE",
    "DELETE FROM",
    "CREATE TABLE",
    "CREATE OR REPLACE TABLE",
    "CREATE VIEW",
    "CREATE OR REPLACE VIEW",
    "CREATE SCHEMA",
    "CREATE MACRO",
    "DROP TABLE",
    "DROP VIEW",
    "ALTER TABLE",
    "DESCRIBE",
    "SUMMARIZE",
    "SHOW TABLES",
    "SHOW ALL TABLES",
    "EXPLAIN",
    "EXPLAIN ANALYZE",
    "PIVOT",
    "UNPIVOT",
    "COPY",
    "ATTACH",
    "DETACH",
    "USE",
    "INSTALL",
    "LOAD",
    "SET",
    "RESET",
    "PRAGMA",
    "CALL",
    "CHECKPOINT",
    "BEGIN",
    "COMMIT",
    "ROLLBACK",
    "TRUNCATE",
    "VACUUM",
    "EXPORT DATABASE",
    "IMPORT DATABASE",
];

/// Keywords that follow a relation or a complete expression.
const CLAUSE_WORDS: &[&str] = &[
    "WHERE",
    "JOIN",
    "LEFT JOIN",
    "INNER JOIN",
    "RIGHT JOIN",
    "FULL OUTER JOIN",
    "CROSS JOIN",
    "ASOF JOIN",
    "POSITIONAL JOIN",
    "ANTI JOIN",
    "SEMI JOIN",
    "NATURAL JOIN",
    "ON",
    "USING",
    "AS",
    "GROUP BY",
    "GROUP BY ALL",
    "ORDER BY",
    "ORDER BY ALL",
    "HAVING",
    "LIMIT",
    "OFFSET",
    "QUALIFY",
    "WINDOW",
    "UNION",
    "UNION ALL",
    "UNION BY NAME",
    "EXCEPT",
    "INTERSECT",
    "SELECT",
    "FROM",
    "SET",
    "VALUES",
    "RETURNING",
    "USING SAMPLE",
];

const EXPR_KWS: &[&str] = &[
    "AND",
    "OR",
    "NOT",
    "IN",
    "IS",
    "IS NULL",
    "IS NOT NULL",
    "NULL",
    "TRUE",
    "FALSE",
    "LIKE",
    "ILIKE",
    "GLOB",
    "SIMILAR TO",
    "BETWEEN",
    "CASE",
    "WHEN",
    "THEN",
    "ELSE",
    "END",
    "DISTINCT",
    "ALL",
    "ANY",
    "EXISTS",
    "CAST",
    "TRY_CAST",
    "INTERVAL",
    "OVER",
    "PARTITION BY",
    "FILTER",
    "ASC",
    "DESC",
    "NULLS FIRST",
    "NULLS LAST",
    "EXCLUDE",
    "REPLACE",
    "COLUMNS",
];

const TYPES: &[&str] = &[
    "VARCHAR",
    "INTEGER",
    "BIGINT",
    "HUGEINT",
    "SMALLINT",
    "TINYINT",
    "UBIGINT",
    "UINTEGER",
    "DOUBLE",
    "FLOAT",
    "DECIMAL",
    "BOOLEAN",
    "DATE",
    "TIME",
    "TIMESTAMP",
    "TIMESTAMPTZ",
    "INTERVAL",
    "UUID",
    "BLOB",
    "JSON",
    "BIT",
    "VARINT",
];

/// Where a FROM item's columns come from.
#[derive(Debug, Clone)]
enum Source {
    Rel(usize),
    Cte(usize),
    /// A subquery: the token range of its body.
    Sub(Range<usize>),
    Unknown,
}

#[derive(Debug)]
struct Ref {
    /// Enclosing `(` token of the FROM clause, `None` at the top level.
    level: Option<usize>,
    alias: Option<String>,
    name: Option<String>,
    src: Source,
}

#[derive(Debug)]
struct Cte {
    name: String,
    /// Explicit `name(a, b)` column names.
    cols: Option<Vec<String>>,
    body: Range<usize>,
}

struct Stmt<'a> {
    src: &'a str,
    toks: &'a [Tok],
    idx: &'a SchemaIndex,
    /// For each token, the index of its enclosing `(`.
    parent: Vec<Option<usize>>,
    /// For each `(`, the index of its matching `)` (or the end).
    close: HashMap<usize, usize>,
    ctes: Vec<Cte>,
    refs: Vec<Ref>,
}

impl<'a> Stmt<'a> {
    fn new(src: &'a str, toks: &'a [Tok], idx: &'a SchemaIndex) -> Self {
        let mut parent = Vec::with_capacity(toks.len());
        let mut close = HashMap::new();
        let mut stack: Vec<usize> = Vec::new();
        for (i, t) in toks.iter().enumerate() {
            if t.t == T::Punct(b')')
                && let Some(open) = stack.pop()
            {
                close.insert(open, i);
            }
            parent.push(stack.last().copied());
            if t.t == T::Punct(b'(') {
                stack.push(i);
            }
        }
        for open in stack {
            close.insert(open, toks.len());
        }
        let mut s = Self {
            src,
            toks,
            idx,
            parent,
            close,
            ctes: Vec::new(),
            refs: Vec::new(),
        };
        s.ctes = s.parse_ctes();
        s.refs = s.parse_refs();
        s
    }

    fn text(&self, i: usize) -> &'a str {
        let t = self.toks[i];
        &self.src[t.start..t.end]
    }

    fn kw(&self, i: usize, word: &str) -> bool {
        self.toks.get(i).is_some_and(|t| t.t == T::Word) && eq(self.text(i), word)
    }

    fn punct(&self, i: usize, c: u8) -> bool {
        self.toks.get(i).is_some_and(|t| t.t == T::Punct(c))
    }

    fn is_stop(&self, word: &str) -> bool {
        STOP_WORDS.iter().any(|w| eq(w, word)) || self.idx.reserved.contains(&word.to_lowercase())
    }

    /// The identifier at `i`, unquoted.
    fn ident(&self, i: usize) -> Option<String> {
        let t = self.toks.get(i)?;
        match t.t {
            T::Word => Some(self.text(i).to_string()),
            T::Quoted { closed } => {
                let inner = &self.src[t.start + 1..t.end - closed as usize];
                Some(inner.replace("\"\"", "\""))
            }
            _ => None,
        }
    }

    /// An identifier that could be an alias: not a clause keyword.
    fn alias_at(&self, i: usize) -> Option<String> {
        let id = self.ident(i)?;
        (self.toks[i].t != T::Word || !self.is_stop(&id)).then_some(id)
    }

    fn close_of(&self, open: usize) -> usize {
        self.close.get(&open).copied().unwrap_or(self.toks.len())
    }

    /// `WITH [RECURSIVE] a [(cols)] AS [NOT] [MATERIALIZED] (…), b AS (…)`
    fn parse_ctes(&self) -> Vec<Cte> {
        let mut out = Vec::new();
        for i in 0..self.toks.len() {
            if !self.kw(i, "with") {
                continue;
            }
            let mut j = i + 1;
            if self.kw(j, "recursive") {
                j += 1;
            }
            while let Some(name) = self.ident(j) {
                j += 1;
                let mut cols = None;
                if self.punct(j, b'(') {
                    let end = self.close_of(j);
                    cols = Some((j + 1..end).filter_map(|k| self.ident(k)).collect());
                    j = end + 1;
                }
                if !self.kw(j, "as") {
                    break;
                }
                j += 1;
                while self.kw(j, "not") || self.kw(j, "materialized") {
                    j += 1;
                }
                if !self.punct(j, b'(') {
                    break;
                }
                let end = self.close_of(j);
                out.push(Cte {
                    name,
                    cols,
                    body: j + 1..end,
                });
                j = end + 1;
                if !self.punct(j, b',') {
                    break;
                }
                j += 1;
            }
        }
        out
    }

    /// Every FROM / JOIN / UPDATE / INTO item in the statement.
    fn parse_refs(&self) -> Vec<Ref> {
        let mut out = Vec::new();
        for i in 0..self.toks.len() {
            if !REL_KWS.iter().any(|k| self.kw(i, k)) {
                continue;
            }
            let from_list = self.kw(i, "from");
            // `INSERT INTO t (a, b)` and `CREATE TABLE t (…)` aren't calls.
            let calls = from_list || self.kw(i, "join");
            let level = self.parent[i];
            let mut j = i + 1;
            loop {
                if self.kw(j, "lateral") {
                    j += 1;
                }
                let mut name = None;
                let (src, mut k) = if self.punct(j, b'(') {
                    let end = self.close_of(j);
                    (Source::Sub(j + 1..end), end + 1)
                } else {
                    let mut parts = Vec::new();
                    let mut k = j;
                    while let Some(p) = self.ident(k) {
                        parts.push(p);
                        if self.punct(k + 1, b'.') && self.ident(k + 2).is_some() {
                            k += 2;
                        } else {
                            k += 1;
                            break;
                        }
                    }
                    if parts.is_empty() {
                        break;
                    }
                    name = parts.last().cloned();
                    if calls && self.punct(k, b'(') {
                        (Source::Unknown, self.close_of(k) + 1)
                    } else if let Some(c) = (parts.len() == 1)
                        .then(|| self.ctes.iter().position(|c| eq(&c.name, &parts[0])))
                        .flatten()
                    {
                        (Source::Cte(c), k)
                    } else {
                        let rel = self.idx.resolve(&parts).map(|r| {
                            self.idx
                                .rels
                                .iter()
                                .position(|x| std::ptr::eq(x, r))
                                .unwrap()
                        });
                        (rel.map_or(Source::Unknown, Source::Rel), k)
                    }
                };
                if self.kw(k, "as") {
                    k += 1;
                }
                let alias = self.alias_at(k);
                if alias.is_some() {
                    k += 1;
                    if self.punct(k, b'(') {
                        k = self.close_of(k) + 1;
                    }
                }
                out.push(Ref {
                    level,
                    alias,
                    name,
                    src,
                });
                if from_list && self.punct(k, b',') {
                    j = k + 1;
                    continue;
                }
                break;
            }
        }
        out
    }

    fn ref_cols(&self, r: &Ref, depth: u8) -> Vec<Col> {
        match &r.src {
            Source::Rel(i) => self.idx.rels[*i].cols.clone(),
            Source::Cte(i) => self.cte_cols(*i, depth),
            Source::Sub(body) => self.outputs(body.clone(), depth + 1),
            Source::Unknown => Vec::new(),
        }
    }

    fn cte_cols(&self, i: usize, depth: u8) -> Vec<Col> {
        let cte = &self.ctes[i];
        match &cte.cols {
            Some(names) => names
                .iter()
                .map(|n| Col {
                    name: n.clone(),
                    ty: String::new(),
                })
                .collect(),
            None => self.outputs(cte.body.clone(), depth + 1),
        }
    }

    /// The column names a `SELECT` body produces, as far as can be told
    /// without running it: aliases, bare columns, and expanded `*`.
    fn outputs(&self, body: Range<usize>, depth: u8) -> Vec<Col> {
        if depth > 4 || body.is_empty() {
            return Vec::new();
        }
        let level = body.start.checked_sub(1);
        let top = |i: usize| self.parent[i] == level;
        let refs: Vec<&Ref> = self.refs.iter().filter(|r| r.level == level).collect();
        let all = |refs: &[&Ref]| -> Vec<Col> {
            refs.iter().flat_map(|r| self.ref_cols(r, depth)).collect()
        };
        let Some(select) = body.clone().find(|&i| top(i) && self.kw(i, "select")) else {
            // FROM-first without a SELECT means every column.
            return all(&refs);
        };
        let ends = [
            "from",
            "where",
            "group",
            "order",
            "limit",
            "union",
            "except",
            "intersect",
        ];
        let end = (select + 1..body.end)
            .find(|&i| top(i) && ends.iter().any(|k| self.kw(i, k)))
            .unwrap_or(body.end);
        let mut start = select + 1;
        while self.kw(start, "distinct") || self.kw(start, "all") {
            start += 1;
        }
        let type_of = |name: &str| {
            refs.iter()
                .flat_map(|r| self.ref_cols(r, depth))
                .find(|c| eq(&c.name, name))
                .map(|c| c.ty)
                .unwrap_or_default()
        };
        let mut out = Vec::new();
        let mut item_start = start;
        for i in start..=end {
            if i < end && !(top(i) && self.punct(i, b',')) {
                continue;
            }
            let item: Vec<usize> = (item_start..i).collect();
            item_start = i + 1;
            let Some(&last) = item.last() else { continue };
            let n = item.len();
            if self.punct(item[0], b'*') {
                out.extend(all(&refs));
            } else if n >= 3 && self.punct(last, b'*') && self.punct(item[n - 2], b'.') {
                let q = self.ident(item[n - 3]).unwrap_or_default();
                if let Some(r) = refs.iter().find(|r| ref_matches(r, &q)) {
                    out.extend(self.ref_cols(r, depth));
                }
            } else if let Some(name) = self.ident(last) {
                let bare = n == 1 || self.punct(item[n - 2], b'.');
                if bare {
                    let ty = type_of(&name);
                    out.push(Col { name, ty });
                } else if n >= 2 {
                    let prev = self.toks[item[n - 2]].t;
                    let aliased = self.kw(item[n - 2], "as")
                        || matches!(prev, T::Punct(b')') | T::Quoted { .. } | T::Str | T::Num)
                        || (prev == T::Word && !self.is_stop(self.text(item[n - 2])));
                    if aliased {
                        out.push(Col {
                            name,
                            ty: String::new(),
                        });
                    }
                }
            }
        }
        out
    }
}

fn ref_matches(r: &Ref, q: &str) -> bool {
    r.alias.as_deref().is_some_and(|a| eq(a, q)) || r.name.as_deref().is_some_and(|n| eq(n, q))
}

// ── matching and ranking ─────────────────────────────────────────────────

/// How well `cand` matches `prefix` (lower is better): 0 prefix, 1 start of
/// a `_` segment, 2 substring, 3 in-order subsequence. Keywords and
/// functions only get the first two, identifiers all four.
fn tier(cand: &str, prefix: &str, loose: bool) -> Option<u8> {
    if prefix.is_empty() {
        return Some(0);
    }
    let cand = cand.to_lowercase();
    if cand.starts_with(prefix) {
        return Some(0);
    }
    if cand.split('_').skip(1).any(|seg| seg.starts_with(prefix)) {
        return Some(1);
    }
    if !loose {
        return None;
    }
    if prefix.len() >= 2 && cand.contains(prefix) {
        return Some(2);
    }
    let mut it = cand.chars();
    (prefix.len() >= 3 && prefix.chars().all(|p| it.any(|c| c == p))).then_some(3)
}

struct Ranked {
    s: Suggestion,
    key: (u8, u8, usize),
}

struct Out<'a> {
    prefix: String,
    range: Range<usize>,
    upper: bool,
    items: Vec<Ranked>,
    seen: HashSet<String>,
    idx: &'a SchemaIndex,
    quoted: bool,
}

impl Out<'_> {
    /// Add a candidate unless something with the same insert text is
    /// already there. `rank` orders kinds, `order` keeps natural order
    /// (column position) among equals.
    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        label: &str,
        insert: String,
        detail: Option<String>,
        kind: Kind,
        rank: u8,
        order: usize,
        loose: bool,
    ) {
        let Some(t) = tier(label, &self.prefix, loose) else {
            return;
        };
        if !self.seen.insert(insert.to_lowercase()) {
            return;
        }
        let matched = if t == 0 && label.is_char_boundary(self.prefix.len().min(label.len())) {
            self.prefix.len().min(label.len())
        } else {
            0
        };
        self.items.push(Ranked {
            s: Suggestion {
                label: label.to_string(),
                insert,
                detail,
                kind,
                range: self.range.clone(),
                matched,
            },
            key: (t, rank, order),
        });
    }

    fn keyword(&mut self, kw: &str, rank: u8) {
        let text = if self.upper {
            kw.to_string()
        } else {
            kw.to_lowercase()
        };
        self.push(&text, text.clone(), None, Kind::Keyword, rank, 0, false);
    }

    fn ident(&self, name: &str) -> String {
        self.idx.ident(name, self.quoted)
    }

    fn finish(mut self) -> Vec<Suggestion> {
        // A word that's already complete closes the menu, so ↵ after typing
        // `FROM` or a full column name makes a newline instead of a pick.
        if !self.prefix.is_empty()
            && self
                .items
                .iter()
                .any(|r| r.s.label.to_lowercase() == self.prefix)
        {
            return Vec::new();
        }
        self.items.sort_by(|a, b| {
            a.key
                .cmp(&b.key)
                .then_with(|| (a.s.label.len(), &a.s.label).cmp(&(b.s.label.len(), &b.s.label)))
        });
        self.items.truncate(MAX_ITEMS);
        self.items.into_iter().map(|r| r.s).collect()
    }
}

// ── entry point ──────────────────────────────────────────────────────────

/// Suggestions for the cursor at byte offset `cursor` in `src`.
pub fn complete(src: &str, cursor: usize, idx: &SchemaIndex) -> Vec<Suggestion> {
    if cursor > src.len() || !src.is_char_boundary(cursor) {
        return Vec::new();
    }
    let Some(all) = lex(src, cursor) else {
        return Vec::new();
    };

    // The statement holding the cursor.
    let semi = |t: &Tok| t.t == T::Punct(b';');
    let lo = all
        .iter()
        .rposition(|t| semi(t) && t.end <= cursor)
        .map_or(0, |i| i + 1);
    let hi = all[lo..]
        .iter()
        .position(|t| semi(t) && t.start >= cursor)
        .map_or(all.len(), |i| lo + i);
    let toks = &all[lo..hi];

    // The word being typed, if any.
    let partial = toks.iter().position(|t| match t.t {
        T::Word => t.start < cursor && cursor <= t.end,
        T::Quoted { closed } => t.start < cursor && (cursor < t.end || !closed),
        _ => false,
    });
    if toks
        .iter()
        .any(|t| t.t == T::Num && t.start < cursor && cursor <= t.end)
    {
        return Vec::new();
    }
    let (prefix, range, quoted, before) = match partial {
        Some(p) => {
            let t = toks[p];
            let quoted = matches!(t.t, T::Quoted { .. });
            let from = t.start + quoted as usize;
            // Swallow an auto-closed quote right after the cursor.
            let end = if quoted && t.end == cursor + 1 && src[cursor..].starts_with('"') {
                t.end
            } else {
                cursor
            };
            (src[from..cursor].to_lowercase(), t.start..end, quoted, p)
        }
        None => (
            String::new(),
            cursor..cursor,
            false,
            toks.iter()
                .position(|t| t.start >= cursor)
                .unwrap_or(toks.len()),
        ),
    };

    let stmt = Stmt::new(src, toks, idx);

    // `a.b.pre|`: the qualifiers written right before the word.
    let mut qual = Vec::new();
    let mut ctx_end = before;
    let mut next = range.start;
    while ctx_end >= 2
        && stmt.punct(ctx_end - 1, b'.')
        && toks[ctx_end - 1].end == next
        && toks[ctx_end - 2].end == toks[ctx_end - 1].start
    {
        let Some(q) = stmt.ident(ctx_end - 2) else {
            break;
        };
        qual.insert(0, q);
        next = toks[ctx_end - 2].start;
        ctx_end -= 2;
    }
    if prefix.is_empty() && qual.is_empty() && !quoted {
        return Vec::new();
    }

    // The clause the cursor is in, at its own paren depth.
    let prev = ctx_end.checked_sub(1);
    let mut clause: Option<usize> = None;
    let mut boundary = false;
    let mut depth = 0;
    for j in (0..ctx_end).rev() {
        if stmt.punct(j, b')') {
            depth += 1;
        } else if stmt.punct(j, b'(') {
            if depth == 0 {
                boundary = true;
                break;
            }
            depth -= 1;
        } else if depth == 0 && CLAUSE_KWS.iter().any(|k| stmt.kw(j, k)) {
            clause = Some(j);
            break;
        }
    }

    // The parens around the cursor: FROM items at those levels are in scope.
    let mut open = Vec::new();
    for j in 0..ctx_end {
        if stmt.punct(j, b'(') {
            open.push(j);
        } else if stmt.punct(j, b')') {
            open.pop();
        }
    }
    let chain = open;
    let visible: Vec<&Ref> = stmt
        .refs
        .iter()
        .filter(|r| r.level.is_none_or(|l| chain.contains(&l)))
        .collect();

    let mut out = Out {
        upper: src[range.start..cursor].chars().any(|c| c.is_uppercase()),
        prefix,
        range,
        items: Vec::new(),
        seen: HashSet::new(),
        idx,
        quoted,
    };

    // `AS alias|` is a new name: nothing to suggest.
    if prev.is_some_and(|p| stmt.kw(p, "as")) && qual.is_empty() {
        return Vec::new();
    }
    // `x::typ|`
    if qual.is_empty()
        && ctx_end >= 2
        && stmt.punct(ctx_end - 1, b':')
        && stmt.punct(ctx_end - 2, b':')
    {
        for (i, t) in TYPES.iter().enumerate() {
            let text = if out.upper {
                t.to_string()
            } else {
                t.to_lowercase()
            };
            out.push(&text, text.clone(), None, Kind::Type, 0, i, false);
        }
        return out.finish();
    }

    let clause_kw = clause.map(|c| stmt.text(c).to_lowercase());
    let in_rel_clause = clause_kw.as_deref().is_some_and(|k| REL_KWS.contains(&k));

    if in_rel_clause {
        let c = clause.unwrap();
        let rel_slot = !qual.is_empty()
            || prev == Some(c)
            || prev.is_some_and(|p| stmt.kw(p, "lateral"))
            || (clause_kw.as_deref() == Some("from") && prev.is_some_and(|p| stmt.punct(p, b',')));
        if rel_slot {
            relations(
                &mut out,
                &stmt,
                &qual,
                clause_kw.as_deref() == Some("from") || clause_kw.as_deref() == Some("join"),
            );
            return out.finish();
        }
        // After a relation: its alias, or the next clause.
        if out.prefix.len() >= 2 {
            for k in CLAUSE_WORDS {
                out.keyword(k, 0);
            }
        }
        return out.finish();
    }

    if clause.is_none() && !boundary {
        // Start of a statement.
        if qual.is_empty() {
            for k in START_KWS {
                out.keyword(k, 0);
            }
        }
        return out.finish();
    }

    if !qual.is_empty() {
        qualified_columns(&mut out, &stmt, &visible, &qual);
        return out.finish();
    }

    // Right after a complete expression (`SELECT a f|`, `WHERE x = 1 a|`)
    // the next word is almost always a keyword, not another column.
    let after_expr = prev.is_some_and(|p| match toks[p].t {
        T::Word => {
            !stmt.is_stop(stmt.text(p))
                && !CLAUSE_KWS.iter().any(|k| stmt.kw(p, k))
                && !EXPR_KWS.iter().any(|k| eq(k, stmt.text(p)))
        }
        T::Quoted { .. } | T::Str | T::Num | T::Punct(b')') | T::Punct(b'*') => true,
        _ => false,
    });
    if after_expr {
        if out.prefix.len() >= 2 {
            for k in CLAUSE_WORDS.iter().chain(EXPR_KWS) {
                out.keyword(k, 0);
            }
        }
        return out.finish();
    }

    // An expression: columns in scope, then tables/aliases, functions, keywords.
    let mut owners: Vec<(Col, Vec<String>)> = Vec::new();
    for r in &visible {
        let owner = r
            .alias
            .clone()
            .or_else(|| r.name.clone())
            .unwrap_or_default();
        for c in stmt.ref_cols(r, 0) {
            match owners.iter_mut().find(|(o, _)| eq(&o.name, &c.name)) {
                Some((_, who)) => who.push(owner.clone()),
                None => owners.push((c, vec![owner.clone()])),
            }
        }
    }
    for (i, (c, who)) in owners.iter().enumerate() {
        let from = if who.len() > 2 {
            format!("{} tables", who.len())
        } else {
            who.join(", ")
        };
        let detail = [c.ty.to_lowercase(), from]
            .into_iter()
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(" · ");
        let insert = out.ident(&c.name);
        out.push(&c.name, insert, Some(detail), Kind::Column, 0, i, true);
    }
    for r in &visible {
        if let Some(a) = r.alias.as_ref().or(r.name.as_ref()) {
            let detail = match (&r.alias, &r.name) {
                (Some(_), Some(n)) => format!("alias · {n}"),
                (Some(_), None) => "subquery".into(),
                _ => "table".into(),
            };
            let insert = out.ident(a);
            out.push(a, insert, Some(detail), Kind::Alias, 1, 0, true);
        }
    }
    if !quoted {
        let in_select = clause_kw.as_deref() == Some("select");
        for (name, kind) in &idx.functions {
            if *kind == FnKind::Table {
                continue;
            }
            let (detail, rank) = match kind {
                FnKind::Aggregate => ("aggregate", if in_select { 2 } else { 3 }),
                _ => ("function", 3),
            };
            out.push(
                name,
                name.clone(),
                Some(detail.into()),
                Kind::Function,
                rank,
                0,
                false,
            );
        }
        for k in EXPR_KWS.iter().chain(CLAUSE_WORDS) {
            out.keyword(k, 2);
        }
    }
    out.finish()
}

/// Relations, schemas and databases for a FROM / JOIN / … slot.
fn relations(out: &mut Out, stmt: &Stmt, qual: &[String], table_fns: bool) {
    let idx = stmt.idx;
    let detail = |r: &Rel, place: bool| {
        let kind = if r.is_view { "view" } else { "table" };
        if place {
            format!("{kind} · {}.{}", r.db, r.schema)
        } else {
            kind.to_string()
        }
    };
    match qual {
        [] => {
            for c in &stmt.ctes {
                let insert = out.ident(&c.name);
                out.push(
                    &c.name,
                    insert,
                    Some("cte".into()),
                    Kind::Relation,
                    0,
                    0,
                    true,
                );
            }
            for r in &idx.rels {
                let (rank, path) = if eq(&r.db, &idx.default_db) {
                    if eq(&r.schema, &idx.default_schema) {
                        (1, vec![&r.name])
                    } else {
                        (2, vec![&r.schema, &r.name])
                    }
                } else if eq(&r.schema, "main") {
                    (3, vec![&r.db, &r.name])
                } else {
                    (3, vec![&r.db, &r.schema, &r.name])
                };
                let insert = path
                    .iter()
                    .map(|p| out.ident(p))
                    .collect::<Vec<_>>()
                    .join(".");
                out.push(
                    &r.name,
                    insert,
                    Some(detail(r, rank > 1)),
                    Kind::Relation,
                    rank,
                    0,
                    true,
                );
            }
            let mut schemas: Vec<&str> = idx
                .rels
                .iter()
                .filter(|r| eq(&r.db, &idx.default_db) && !eq(&r.schema, &idx.default_schema))
                .map(|r| r.schema.as_str())
                .collect();
            schemas.dedup();
            for s in schemas {
                let insert = out.ident(s);
                out.push(s, insert, Some("schema".into()), Kind::Schema, 4, 0, true);
            }
            for d in &idx.databases {
                if !eq(d, &idx.default_db) {
                    let insert = out.ident(d);
                    out.push(
                        d,
                        insert,
                        Some("database".into()),
                        Kind::Database,
                        4,
                        0,
                        true,
                    );
                }
            }
            if table_fns && !out.quoted {
                for (name, kind) in &idx.functions {
                    if *kind == FnKind::Table {
                        out.push(
                            name,
                            name.clone(),
                            Some("table function".into()),
                            Kind::Function,
                            5,
                            0,
                            false,
                        );
                    }
                }
            }
        }
        [q] => {
            // `schema.` in the default database, or `db.` (its schemas, and
            // `db.name` for its main schema).
            for r in &idx.rels {
                if eq(&r.db, &idx.default_db) && eq(&r.schema, q) {
                    let insert = out.ident(&r.name);
                    out.push(
                        &r.name,
                        insert,
                        Some(detail(r, false)),
                        Kind::Relation,
                        0,
                        0,
                        true,
                    );
                }
            }
            if idx.is_database(q) {
                for r in &idx.rels {
                    if eq(&r.db, q) && eq(&r.schema, "main") {
                        let insert = out.ident(&r.name);
                        out.push(
                            &r.name,
                            insert,
                            Some(detail(r, false)),
                            Kind::Relation,
                            1,
                            0,
                            true,
                        );
                    }
                }
                let mut schemas: Vec<&str> = idx
                    .rels
                    .iter()
                    .filter(|r| eq(&r.db, q) && !eq(&r.schema, "main"))
                    .map(|r| r.schema.as_str())
                    .collect();
                schemas.dedup();
                for s in schemas {
                    let insert = out.ident(s);
                    out.push(s, insert, Some("schema".into()), Kind::Schema, 2, 0, true);
                }
            }
        }
        [d, s] => {
            for r in &idx.rels {
                if eq(&r.db, d) && eq(&r.schema, s) {
                    let insert = out.ident(&r.name);
                    out.push(
                        &r.name,
                        insert,
                        Some(detail(r, false)),
                        Kind::Relation,
                        0,
                        0,
                        true,
                    );
                }
            }
        }
        _ => {}
    }
}

/// `alias.col|`, `table.col|`, `schema.table.col|`.
fn qualified_columns(out: &mut Out, stmt: &Stmt, visible: &[&Ref], qual: &[String]) {
    let cols = match qual {
        [q] => visible
            .iter()
            .rev()
            .copied()
            .find(|r| ref_matches(r, q))
            .or_else(|| stmt.refs.iter().find(|r| ref_matches(r, q)))
            .map(|r| stmt.ref_cols(r, 0))
            .or_else(|| stmt.idx.resolve(qual).map(|r| r.cols.clone())),
        _ => stmt.idx.resolve(qual).map(|r| r.cols.clone()),
    };
    if let Some(cols) = cols {
        for (i, c) in cols.iter().enumerate() {
            let insert = out.ident(&c.name);
            let detail = (!c.ty.is_empty()).then(|| c.ty.to_lowercase());
            out.push(&c.name, insert, detail, Kind::Column, 0, i, true);
        }
    } else if let [q] = qual {
        // Not a table: maybe `schema.` or `db.` written in an expression.
        if stmt.idx.is_schema(&stmt.idx.default_db, q) || stmt.idx.is_database(q) {
            relations(out, stmt, qual, false);
        }
    }
}

// ── editor glue ──────────────────────────────────────────────────────────

/// The editor's completion provider. The workspace swaps in a fresh index
/// whenever the catalog or the focused database changes.
#[derive(Default)]
pub struct SqlCompletions {
    index: RefCell<Rc<SchemaIndex>>,
}

impl SqlCompletions {
    pub fn set_index(&self, index: SchemaIndex) {
        *self.index.borrow_mut() = Rc::new(index);
    }

    /// Menu items for the cursor at byte `offset`.
    pub fn items(&self, text: &Rope, offset: usize) -> Vec<CompletionItem> {
        let src = text.to_string();
        let index = self.index.borrow().clone();
        complete(&src, offset, &index)
            .into_iter()
            .map(|s| CompletionItem {
                filter_text: Some(s.label[..s.matched].to_string()),
                label: s.label,
                detail: s.detail,
                kind: Some(match s.kind {
                    Kind::Column => CompletionItemKind::FIELD,
                    Kind::Alias | Kind::Relation => CompletionItemKind::CLASS,
                    Kind::Schema | Kind::Database => CompletionItemKind::MODULE,
                    Kind::Function => CompletionItemKind::FUNCTION,
                    Kind::Keyword => CompletionItemKind::KEYWORD,
                    Kind::Type => CompletionItemKind::TYPE_PARAMETER,
                }),
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: lsp_types::Range::new(
                        text.offset_to_position(s.range.start),
                        text.offset_to_position(s.range.end),
                    ),
                    new_text: s.insert,
                })),
                ..Default::default()
            })
            .collect()
    }
}

impl CompletionProvider for SqlCompletions {
    fn completions(
        &self,
        text: &Rope,
        offset: usize,
        _: CompletionContext,
        _: &mut Window,
        _: &mut App,
    ) -> Task<anyhow::Result<CompletionResponse>> {
        Task::ready(Ok(CompletionResponse::Array(self.items(text, offset))))
    }

    /// Every keystroke re-ranks (or closes) the menu; pastes don't open it.
    fn is_completion_trigger(&self, _: usize, new_text: &str, _: &mut App) -> bool {
        new_text.chars().count() <= 2
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quack::{Relation, TableColumn};

    fn index() -> SchemaIndex {
        let mut cat = Catalog {
            current_database: Some("memory".into()),
            current_schema: Some("main".into()),
            databases: vec!["memory".into(), "lake".into()],
            reserved: vec!["select".into(), "order".into(), "from".into()],
            functions: vec![
                ("count".into(), "aggregate".into()),
                ("coalesce".into(), "scalar".into()),
                ("date_trunc".into(), "scalar".into()),
                ("read_parquet".into(), "table".into()),
                ("+".into(), "scalar".into()),
            ],
            ..Default::default()
        };
        let tables: &[(&str, &str, &str, &[(&str, &str)])] = &[
            (
                "memory",
                "main",
                "users",
                &[
                    ("id", "INTEGER"),
                    ("name", "VARCHAR"),
                    ("created_at", "TIMESTAMP"),
                ],
            ),
            (
                "memory",
                "main",
                "orders",
                &[
                    ("id", "INTEGER"),
                    ("user_id", "INTEGER"),
                    ("total", "DOUBLE"),
                    ("order", "VARCHAR"),
                ],
            ),
            (
                "memory",
                "analytics",
                "events",
                &[("event_id", "BIGINT"), ("kind", "VARCHAR")],
            ),
            (
                "lake",
                "main",
                "trips",
                &[("trip_id", "BIGINT"), ("Fare Amount", "DOUBLE")],
            ),
        ];
        for (db, schema, name, cols) in tables {
            cat.relations.push(Relation {
                database: db.to_string(),
                schema: schema.to_string(),
                name: name.to_string(),
                is_view: false,
                estimated_rows: None,
                columns: None,
            });
            for (c, ty) in cols.iter() {
                cat.columns.push(TableColumn {
                    database: db.to_string(),
                    schema: schema.to_string(),
                    table: name.to_string(),
                    name: c.to_string(),
                    data_type: ty.to_string(),
                });
            }
        }
        SchemaIndex::new(&cat, None)
    }

    /// Complete at the `|` in `sql`; returns the insert texts.
    fn at(sql: &str) -> Vec<String> {
        let cursor = sql.find('|').expect("cursor");
        let src = sql.replacen('|', "", 1);
        complete(&src, cursor, &index())
            .into_iter()
            .map(|s| s.insert)
            .collect()
    }

    #[test]
    fn tables_after_from() {
        let got = at("SELECT * FROM u|");
        assert_eq!(got[0], "users");
        let got = at("select * from ev|");
        assert_eq!(got[0], "analytics.events", "other schemas are qualified");
        let got = at("SELECT * FROM tr|");
        assert_eq!(got[0], "lake.trips", "other databases are qualified");
        let got = at("SELECT * FROM users, or|");
        assert_eq!(got[0], "orders");
        let got = at("SELECT * FROM users u JOIN o|");
        assert_eq!(got[0], "orders");
        assert!(at("SELECT * FROM read_p|").contains(&"read_parquet".to_string()));
    }

    #[test]
    fn qualified_relations() {
        assert_eq!(at("SELECT * FROM analytics.|"), vec!["events"]);
        assert_eq!(at("SELECT * FROM lake.|"), vec!["trips"]);
        assert_eq!(at("SELECT * FROM lake.main.t|"), vec!["trips"]);
    }

    #[test]
    fn columns_from_scope() {
        let got = at("SELECT na| FROM users");
        assert_eq!(got[0], "name");
        let got = at("SELECT * FROM users WHERE cr|");
        assert_eq!(got[0], "created_at");
        // Only tables in the query contribute columns.
        assert!(!at("SELECT * FROM users WHERE to|").contains(&"total".to_string()));
        let got = at("SELECT * FROM users u JOIN orders o ON o.user_id = u.i|");
        assert_eq!(got, vec!["id"]);
    }

    #[test]
    fn alias_dot_lists_columns_in_order() {
        assert_eq!(
            at("SELECT o.| FROM orders o"),
            vec!["id", "user_id", "total", "\"order\""]
        );
        assert_eq!(
            at("SELECT users.| FROM users"),
            vec!["id", "name", "created_at"]
        );
        // Even before the FROM exists.
        assert_eq!(at("SELECT users.n|"), vec!["name"]);
    }

    #[test]
    fn quoting() {
        let got = at("SELECT t.| FROM lake.trips t");
        assert!(got.contains(&"\"Fare Amount\"".to_string()));
        let got = at("SELECT \"Fa|\" FROM lake.trips");
        assert_eq!(got, vec!["\"Fare Amount\""]);
        // Typed quote, auto-closed: the range swallows the closing quote.
        let src = "SELECT \"Fa\" FROM lake.trips";
        let s = &complete(src, 10, &index())[0];
        assert_eq!(&src[s.range.clone()], "\"Fa\"");
    }

    #[test]
    fn ctes_and_subqueries() {
        let sql = "WITH big AS (SELECT id, total AS amount, count(*) n FROM orders) SELECT big.| FROM big";
        assert_eq!(at(sql), vec!["id", "amount", "n"]);
        let sql = "WITH b AS (FROM orders) SELECT u| FROM b";
        assert_eq!(at(sql)[0], "user_id");
        let sql = "SELECT s.| FROM (SELECT name AS who FROM users) s";
        assert_eq!(at(sql), vec!["who"]);
        assert!(at("WITH recent AS (SELECT 1) SELECT * FROM rec|").contains(&"recent".to_string()));
    }

    #[test]
    fn keywords_by_position() {
        assert_eq!(at("sel|")[0], "select");
        assert_eq!(at("SEL|")[0], "SELECT");
        assert_eq!(at("SELECT id, name fr|")[0], "from", "after an expression");
        assert_eq!(at("SELECT * FROM users wh|")[0], "where");
        assert_eq!(at("SELECT * FROM users ORDER BY id de|")[0], "desc");
        assert_eq!(at("SELECT x::var|")[0], "varchar");
    }

    #[test]
    fn functions_in_expressions() {
        let got = at("SELECT co| FROM users");
        assert_eq!(got[0], "count", "aggregates lead in SELECT");
        assert!(got.contains(&"coalesce".to_string()));
        assert!(at("SELECT tru| FROM users").contains(&"date_trunc".to_string()));
    }

    #[test]
    fn quiet_places() {
        assert!(at("SELECT 'us|' FROM users").is_empty(), "inside a string");
        assert!(at("SELECT 1 -- fr|").is_empty(), "inside a comment");
        assert!(at("SELECT count(*) AS to|").is_empty(), "naming an alias");
        assert!(
            at("SELECT * FROM users u|").is_empty(),
            "short alias after a table"
        );
        assert!(at("SELECT * FROM users|").is_empty(), "already complete");
        assert!(at("SELECT 12|").is_empty());
        assert!(at("SELECT |").is_empty());
    }

    #[test]
    fn statements_are_separate() {
        let got = at("SELECT * FROM orders; SELECT to| FROM users");
        assert!(!got.contains(&"total".to_string()));
        let got = at("SELECT to| FROM orders; SELECT * FROM users");
        assert_eq!(got[0], "total");
    }

    /// The real catalog query, through a real DuckDB, feeds the index.
    #[test]
    fn catalog_feeds_completions() {
        let dir = std::env::temp_dir().join(format!("duckplus-complete-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.duckdb");
        duckdb::Connection::open(&path)
            .unwrap()
            .execute_batch(
                "CREATE SCHEMA sales; \
                 CREATE TABLE sales.invoices (invoice_id INT, \"Due Date\" DATE, \"order\" INT); \
                 CREATE VIEW recent AS SELECT invoice_id FROM sales.invoices;",
            )
            .unwrap();
        let (client, _) = crate::quack::QuackClient::connect_local(&path, true).unwrap();
        let idx = SchemaIndex::new(&client.catalog().unwrap(), None);
        let run = |sql: &str| {
            let cursor = sql.find('|').unwrap();
            let src = sql.replacen('|', "", 1);
            complete(&src, cursor, &idx)
                .into_iter()
                .map(|s| s.insert)
                .collect::<Vec<_>>()
        };
        assert_eq!(run("FROM inv|")[0], "sales.invoices");
        assert_eq!(run("SELECT * FROM rec|")[0], "recent");
        assert_eq!(
            run("SELECT i.| FROM sales.invoices i"),
            vec!["invoice_id", "\"Due Date\"", "\"order\""],
            "column order, quoting, and reserved words come from the server"
        );
        assert_eq!(run("SELECT string_ag|")[0], "string_agg");
        assert_eq!(run("SELECT * FROM read_cs|")[0], "read_csv");
        drop(client);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuzzy_identifiers() {
        assert_eq!(
            at("SELECT * FROM users WHERE at|")[0],
            "created_at",
            "segment match"
        );
        assert_eq!(
            at("SELECT * FROM orders WHERE usid|")[0],
            "user_id",
            "subsequence"
        );
    }
}
