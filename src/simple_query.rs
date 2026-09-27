//! Recognize queries whose result rows map 1:1 onto a single table's rows,
//! so the grid can edit them like a table opened from the sidebar:
//!
//!   SELECT * | col, … FROM [[db.]schema.]table [[AS] alias]
//!          [WHERE …] [ORDER BY …] [LIMIT …] [OFFSET …]
//!
//! (or DuckDB's FROM-first form). Anything that could reshape rows or rename
//! columns — joins, grouping, DISTINCT, set operations, CTEs, table
//! functions, expressions or aliases in the select list — is rejected, so a
//! result column is always the table column of the same name.

/// Keywords that, at the top level, mean the result isn't plain table rows.
const RESHAPING: &[&str] = &[
    "JOIN",
    "GROUP",
    "HAVING",
    "UNION",
    "INTERSECT",
    "EXCEPT",
    "WINDOW",
    "QUALIFY",
    "DISTINCT",
    "PIVOT",
    "UNPIVOT",
    "POSITIONAL",
    "ASOF",
    "NATURAL",
    "CROSS",
    "LATERAL",
    "WITH",
    "INTO",
    "VALUES",
    "EXCLUDE",
    "REPLACE",
    "RENAME",
    "COLUMNS",
    "TABLESAMPLE",
];

/// Clauses that may follow the table without changing which rows mean what.
const TRAILING: &[&str] = &["WHERE", "ORDER", "LIMIT", "OFFSET", "USING"];

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    /// Bare word, uppercased for keyword checks (original kept).
    Word(String, String),
    /// "quoted identifier"
    Quoted(String),
    Sym(char),
    /// Strings, numbers and anything else irrelevant to the shape.
    Other,
}

fn tokenize(sql: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i += 2;
            while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i += 2;
        } else if c == '\'' || c == '"' {
            let mut text = String::new();
            i += 1;
            while i < chars.len() {
                if chars[i] == c {
                    if chars.get(i + 1) == Some(&c) {
                        text.push(c);
                        i += 2;
                        continue;
                    }
                    break;
                }
                text.push(chars[i]);
                i += 1;
            }
            i += 1;
            out.push(if c == '"' {
                Tok::Quoted(text)
            } else {
                Tok::Other
            });
        } else if c.is_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len()
                && (chars[i].is_alphanumeric() || chars[i] == '_' || chars[i] == '$')
            {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            out.push(Tok::Word(word.to_uppercase(), word));
        } else if c.is_ascii_digit() {
            while i < chars.len() && (chars[i].is_alphanumeric() || chars[i] == '.') {
                i += 1;
            }
            out.push(Tok::Other);
        } else {
            out.push(Tok::Sym(c));
            i += 1;
        }
    }
    out
}

fn is_kw(t: &Tok, kw: &str) -> bool {
    matches!(t, Tok::Word(up, _) if up == kw)
}

fn ident(t: &Tok) -> Option<&str> {
    match t {
        Tok::Word(_, w) => Some(w),
        Tok::Quoted(q) => Some(q),
        _ => None,
    }
}

/// `a.b.c` → ["a", "b", "c"]; `None` unless the tokens are exactly that.
fn dotted(tokens: &[&Tok]) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    for (ix, t) in tokens.iter().enumerate() {
        if ix % 2 == 0 {
            parts.push(ident(t)?.to_string());
        } else if **t != Tok::Sym('.') {
            return None;
        }
    }
    (tokens.len() % 2 == 1).then_some(parts)
}

/// The table (as written: `[db.][schema.]name`) a simple query reads, or
/// `None` if its rows or columns might not map 1:1 onto that table's.
pub fn single_table(sql: &str) -> Option<Vec<String>> {
    let tokens = tokenize(sql);
    // Tokens at paren depth 0, with a trailing `;` dropped.
    let mut depth = 0i32;
    let mut top: Vec<&Tok> = Vec::new();
    for t in &tokens {
        match t {
            Tok::Sym('(') => {
                if depth == 0 {
                    top.push(t);
                }
                depth += 1;
            }
            Tok::Sym(')') => depth -= 1,
            _ if depth == 0 => top.push(t),
            _ => {}
        }
    }
    while top.last() == Some(&&Tok::Sym(';')) {
        top.pop();
    }
    if depth != 0 || top.iter().any(|t| **t == Tok::Sym(';')) {
        return None; // unbalanced, or more than one statement
    }
    if top.iter().any(|t| RESHAPING.iter().any(|kw| is_kw(t, kw))) {
        return None;
    }

    let pos = |kw: &str| top.iter().position(|t| is_kw(t, kw));
    let (select, from) = (pos("SELECT"), pos("FROM")?);
    let trailing = top
        .iter()
        .position(|t| TRAILING.iter().any(|kw| is_kw(t, kw)))
        .unwrap_or(top.len());

    let (select_list, from_clause) = match select {
        // SELECT … FROM t …
        Some(0) if from < trailing => (Some(&top[1..from]), &top[from + 1..trailing]),
        // FROM t [SELECT …] …
        Some(s) if from == 0 && s < trailing => (Some(&top[s + 1..trailing]), &top[1..s]),
        None if from == 0 => (None, &top[1..trailing]),
        _ => return None,
    };

    // FROM clause: a dotted name plus an optional alias; no commas (joins)
    // and no parens (table functions, subqueries).
    let (name_len, alias) = match from_clause {
        [.., t, a] if is_kw(t, "AS") => (from_clause.len() - 2, ident(a)),
        [.., prev, a] if **prev != Tok::Sym('.') && ident(a).is_some() && from_clause.len() > 1 => {
            (from_clause.len() - 1, ident(a))
        }
        _ => (from_clause.len(), None),
    };
    let table = dotted(&from_clause[..name_len]).filter(|p| (1..=3).contains(&p.len()))?;

    // Select list: `*`, or plain (optionally qualified) columns.
    if let Some(list) = select_list {
        if list.is_empty() {
            return None;
        }
        let name = table.last().unwrap();
        for item in list.split(|t| **t == Tok::Sym(',')) {
            let ok = match item {
                [Tok::Sym('*')] => true,
                [q, Tok::Sym('.'), Tok::Sym('*')] => ident(q).is_some(),
                _ => match dotted(item) {
                    Some(parts) if parts.len() == 1 => true,
                    // `alias.col` / `table.col`
                    Some(parts) if parts.len() == 2 => {
                        let q = &parts[0];
                        q.eq_ignore_ascii_case(name)
                            || alias.is_some_and(|a| q.eq_ignore_ascii_case(a))
                    }
                    _ => false,
                },
            };
            if !ok {
                return None;
            }
        }
    }
    Some(table)
}

#[cfg(test)]
mod tests {
    use super::single_table;

    fn t(sql: &str) -> Option<Vec<&'static str>> {
        single_table(sql).map(|v| v.into_iter().map(|s| &*s.leak()).collect())
    }

    #[test]
    fn simple_queries_are_editable() {
        assert_eq!(t("select * from images limit 5;"), Some(vec!["images"]));
        assert_eq!(
            t("SELECT id, url FROM main.images WHERE id > 10 ORDER BY id DESC LIMIT 5 OFFSET 5"),
            Some(vec!["main", "images"])
        );
        assert_eq!(
            t("from lake.main.images"),
            Some(vec!["lake", "main", "images"])
        );
        assert_eq!(
            t("from images select id, url where url like '%.png'"),
            Some(vec!["images"])
        );
        assert_eq!(t("select i.* from images as i"), Some(vec!["images"]));
        assert_eq!(t("select i.id, i.url from images i"), Some(vec!["images"]));
        assert_eq!(
            t("select \"Id\" from \"My Images\""),
            Some(vec!["My Images"])
        );
        // Subqueries in WHERE don't change the result's shape.
        assert_eq!(
            t("select * from images where id in (select image_id from tags join x using (id))"),
            Some(vec!["images"])
        );
        assert_eq!(
            t("-- latest\nselect * from images /* all */ limit 5"),
            Some(vec!["images"])
        );
        assert_eq!(
            t("select * from images using sample 10"),
            Some(vec!["images"])
        );
    }

    #[test]
    fn anything_reshaping_is_not() {
        for sql in [
            "select * from images join tags on tags.id = images.id",
            "select * from images, tags",
            "select count(*) from images",
            "select url as id from images",
            "select id, upper(url) from images",
            "select distinct url from images",
            "select url from images group by url",
            "select * from images union all select * from images",
            "with x as (select 1) select * from x",
            "select * from read_parquet('a.parquet')",
            "select * from (select * from images)",
            "select * exclude (url) from images",
            "select * replace (upper(url) as url) from images",
            "select x.id from images i",
            "select * from images; select 1",
            "update images set url = 'x'",
            "select 1",
        ] {
            assert_eq!(t(sql), None, "{sql}");
        }
    }
}
