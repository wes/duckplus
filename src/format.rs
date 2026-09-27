//! SQL formatting (⌘L), tuned for DuckDB's Postgres-flavored syntax.
//!
//! Only layout changes: keyword case is left alone (uppercasing would also
//! hit identifiers that happen to be keywords, like `events` or `date`), and
//! the result is rejected if anything but whitespace differs.

use sqlformat::{Dialect, FormatOptions, Indent, QueryParams};

/// Format SQL, or `None` if the formatter would change more than whitespace.
pub fn format_sql(sql: &str) -> Option<String> {
    let options = FormatOptions {
        indent: Indent::Spaces(2),
        uppercase: None,
        lines_between_queries: 2,
        // Short argument lists (SELECT a, b, c) stay on one line.
        max_inline_arguments: Some(60),
        max_inline_block: 60,
        dialect: Dialect::PostgreSql,
        ..FormatOptions::default()
    };
    // The formatter doesn't know dollar-quoted strings; hide them in plain
    // string literals while it runs.
    let (masked, strings) = mask_dollar_strings(sql);
    let mut out = sqlformat::format(&masked, &QueryParams::None, &options);
    for (ix, s) in strings.iter().enumerate().rev() {
        out = out.replace(&placeholder(ix), s);
    }
    let out = if sql.ends_with('\n') {
        format!("{}\n", out.trim_end())
    } else {
        out.trim_end().to_string()
    };
    (squash(&out) == squash(sql)).then_some(out)
}

fn squash(s: &str) -> String {
    s.chars().filter(|c| !c.is_whitespace()).collect()
}

fn placeholder(ix: usize) -> String {
    format!("'__duckplus_dollar_{ix}__'")
}

/// Replace `$$…$$` / `$tag$…$tag$` strings with placeholder literals.
fn mask_dollar_strings(sql: &str) -> (String, Vec<String>) {
    let mut out = String::new();
    let mut strings = Vec::new();
    let mut rest = sql;
    while let Some(start) = rest.find('$') {
        let after = &rest[start + 1..];
        let tag_len = after
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(after.len());
        // `$1` style parameters aren't dollar quotes.
        let is_tag = after[..tag_len]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_digit());
        if is_tag && after[tag_len..].starts_with('$') {
            let delim = &rest[start..start + tag_len + 2];
            let body = &rest[start + delim.len()..];
            if let Some(end) = body.find(delim) {
                out.push_str(&rest[..start]);
                out.push_str(&placeholder(strings.len()));
                strings.push(rest[start..start + delim.len() * 2 + end].to_string());
                rest = &body[end + delim.len()..];
                continue;
            }
        }
        out.push_str(&rest[..=start]);
        rest = &rest[start + 1..];
    }
    out.push_str(rest);
    (out, strings)
}

#[cfg(test)]
mod tests {
    use super::format_sql;

    #[test]
    fn lays_out_clauses_and_keeps_case() {
        let out = format_sql(
            "select a, b, count(*) as n from events e join users u on u.id = e.user_id \
             where e.ts > now() - interval 1 day group by all order by n desc limit 10;",
        )
        .unwrap();
        assert!(
            out.starts_with("select\n  a, b, count(*) as n\nfrom\n  events e"),
            "{out}"
        );
    }

    #[test]
    fn duckdb_syntax_survives() {
        for sql in [
            "from users select id, name where age > 30",
            "select data->>'$.name' as name, [1,2,3] as l, x::int, list_transform(l, x -> x + 1) from t; select 1",
            "-- keep me\nselect 'it''s; fine' as s, $$raw;  text$$ as d /* block */ from t",
            "select $tag$a $$ b$tag$ as x, $1 as p",
            "select * exclude (a), columns('^x') from quack_query('h', 'select 1', token := 'x')",
            "with recent as (select * from events where ts > '2024-01-01') select user_id, count(*) from recent group by 1 having count(*) > 5",
        ] {
            let out = format_sql(sql).unwrap_or_else(|| panic!("refused: {sql}"));
            // Dollar-quoted bodies come back untouched.
            if let Some(i) = sql.find("$$raw") {
                assert!(out.contains(&sql[i..i + 15]), "{out}");
            }
        }
    }

    #[test]
    fn keeps_trailing_newline() {
        assert!(format_sql("select 1\n").unwrap().ends_with("1\n"));
        assert!(format_sql("select 1").unwrap().ends_with('1'));
    }
}
