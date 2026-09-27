//! Database client: a remote Quack server or a local DuckDB file.
//!
//! For Quack, DuckPlus embeds an in-memory DuckDB whose only job is to speak
//! the protocol. Every statement is shipped to the server through
//! `quack_query(endpoint, sql, token := ...)`, so the full remote dialect is
//! available. Local files are opened directly by the embedded engine.
//! Either way results arrive as Arrow batches, formatted lazily — only the
//! cells on screen are ever turned into strings.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, LazyLock, Mutex, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow, bail};
use duckdb::arrow::array::{Array, RecordBatch};
use duckdb::arrow::compute::{SortOptions, cast, concat, sort_to_indices};
use duckdb::arrow::datatypes::{DataType, Schema};
use duckdb::arrow::util::display::{ArrayFormatter, FormatOptions};
use duckdb::{AccessMode, Config, Connection, InterruptHandle};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum TlsMode {
    /// Let Quack decide: plain HTTP for localhost, HTTPS everywhere else.
    #[default]
    Auto,
    /// Force plain HTTP (e.g. a server on a private network).
    Disabled,
}

/// Normalize whatever the user pasted into a `quack:host[:port]` URI.
pub fn normalize_endpoint(input: &str) -> String {
    let s = input.trim().trim_end_matches('/');
    let s = s
        .strip_prefix("quack://")
        .or_else(|| s.strip_prefix("quack:"))
        .or_else(|| s.strip_prefix("https://"))
        .or_else(|| s.strip_prefix("http://"))
        .unwrap_or(s);
    let s = s.trim_end_matches("/quack");
    if s.is_empty() {
        "quack:localhost".into()
    } else {
        format!("quack:{s}")
    }
}

pub fn sql_literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[derive(Clone)]
pub struct QuackClient {
    inner: Arc<Inner>,
}

struct Remote {
    endpoint: String,
    token: String,
    tls: TlsMode,
}

/// Local databases already open in this process, so a second window (or a
/// second saved connection) on the same file shares it instead of failing on
/// DuckDB's file lock.
static LOCAL: LazyLock<Mutex<HashMap<(PathBuf, bool), Weak<Inner>>>> =
    LazyLock::new(Default::default);

struct Inner {
    /// `None` for a local database file.
    remote: Option<Remote>,
    /// Template connection; every user query runs on its own clone so a
    /// cancelled (abandoned) query never blocks the next one.
    base: Mutex<Connection>,
    /// Separate connection so schema/admin lookups never wait behind a long query.
    meta: Mutex<Connection>,
    /// Interrupt handle of the query currently in flight.
    running: Mutex<Option<Arc<InterruptHandle>>>,
}

#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub version: String,
    pub codename: String,
    pub latency: Duration,
}

impl QuackClient {
    /// Spin up the local engine, load Quack and verify the endpoint + token.
    pub fn connect(endpoint: &str, token: &str, tls: TlsMode) -> Result<(Self, ServerInfo)> {
        let conn = Connection::open_in_memory().context("failed to start embedded DuckDB")?;
        conn.execute_batch("INSTALL quack; LOAD quack;")
            .context("failed to load the quack extension")?;
        let meta = conn.try_clone()?;
        let client = Self {
            inner: Arc::new(Inner {
                remote: Some(Remote {
                    endpoint: normalize_endpoint(endpoint),
                    token: token.to_string(),
                    tls,
                }),
                base: Mutex::new(conn),
                meta: Mutex::new(meta),
                running: Mutex::new(None),
            }),
        };
        let info = client.ping()?;
        Ok((client, info))
    }

    /// Open a DuckDB database file in-process.
    pub fn connect_local(path: &Path, read_only: bool) -> Result<(Self, ServerInfo)> {
        let path = path
            .canonicalize()
            .with_context(|| format!("{} not found", path.display()))?;
        let key = (path.clone(), read_only);
        let existing = LOCAL.lock().unwrap().get(&key).and_then(Weak::upgrade);
        let client = match existing {
            Some(inner) => Self { inner }.fork()?,
            None => {
                let mode = if read_only {
                    AccessMode::ReadOnly
                } else {
                    AccessMode::ReadWrite
                };
                let conn = Connection::open_with_flags(&path, Config::default().access_mode(mode)?)
                    .map_err(|e| anyhow!("{e}"))?;
                let meta = conn.try_clone()?;
                Self {
                    inner: Arc::new(Inner {
                        remote: None,
                        base: Mutex::new(conn),
                        meta: Mutex::new(meta),
                        running: Mutex::new(None),
                    }),
                }
            }
        };
        LOCAL
            .lock()
            .unwrap()
            .insert(key, Arc::downgrade(&client.inner));
        let info = client.ping()?;
        Ok((client, info))
    }

    pub fn is_local(&self) -> bool {
        self.inner.remote.is_none()
    }

    /// A second handle on the same database with its own connections, so its
    /// queries (and cancels) never interfere with this one's.
    pub fn fork(&self) -> Result<Self> {
        let base = self.inner.base.lock().unwrap().try_clone()?;
        let meta = base.try_clone()?;
        Ok(Self {
            inner: Arc::new(Inner {
                remote: self.inner.remote.as_ref().map(|r| Remote {
                    endpoint: r.endpoint.clone(),
                    token: r.token.clone(),
                    tls: r.tls,
                }),
                base: Mutex::new(base),
                meta: Mutex::new(meta),
                running: Mutex::new(None),
            }),
        })
    }

    pub fn ping(&self) -> Result<ServerInfo> {
        let started = Instant::now();
        let rows = self.meta_rows("SELECT library_version, codename FROM pragma_version()")?;
        let row = rows
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty version response"))?;
        Ok(ServerInfo {
            version: row.first().cloned().flatten().unwrap_or_default(),
            codename: row.get(1).cloned().flatten().unwrap_or_default(),
            latency: started.elapsed(),
        })
    }

    fn wrap(&self, sql: &str) -> String {
        let Some(remote) = &self.inner.remote else {
            return sql.to_string();
        };
        let mut q = format!(
            "SELECT * FROM quack_query({}, {}, token := {}",
            sql_literal(&remote.endpoint),
            sql_literal(sql),
            sql_literal(&remote.token),
        );
        if remote.tls == TlsMode::Disabled {
            q.push_str(", disable_ssl := true");
        }
        q.push(')');
        q
    }

    /// Run user SQL, with `database` (if any) as the default catalog. Stops
    /// pulling batches once `max_rows` is reached.
    pub fn run(&self, sql: &str, max_rows: usize, database: Option<&str>) -> Result<QueryResult> {
        let started = Instant::now();
        let conn = self.inner.base.lock().unwrap().try_clone()?;
        *self.inner.running.lock().unwrap() = Some(conn.interrupt_handle());
        let scoped;
        let sql = match database {
            // Every query runs on a fresh connection (and Quack calls are
            // stateless), so the USE has to travel with each one.
            Some(db) if self.is_local() => {
                conn.execute_batch(&format!("USE {}", quote_ident(db)))
                    .map_err(|e| self.clean_error(e))?;
                sql
            }
            Some(db) => {
                scoped = format!("USE {};\n{sql}", quote_ident(db));
                &scoped
            }
            None => sql,
        };
        let wrapped = if self.is_local() {
            // quack_query takes whole scripts; locally only one statement can
            // be prepared, so run the leading ones first and show the last.
            let mut statements = split_statements(sql);
            let last = statements.pop().unwrap_or_else(|| sql.to_string());
            if !statements.is_empty() {
                conn.execute_batch(&statements.join(";\n"))
                    .map_err(|e| self.clean_error(e))?;
            }
            last
        } else {
            self.wrap(sql)
        };
        let mut stmt = conn.prepare(&wrapped).map_err(|e| self.clean_error(e))?;
        let arrow = stmt.query_arrow([]).map_err(|e| self.clean_error(e))?;
        let schema = arrow.get_schema();
        let mut batches = Vec::new();
        let mut offsets = Vec::new();
        let mut total = 0usize;
        let mut truncated = false;
        for batch in arrow {
            if total >= max_rows {
                truncated = true;
                break;
            }
            let take = (max_rows - total).min(batch.num_rows());
            let batch = if take < batch.num_rows() {
                truncated = true;
                batch.slice(0, take)
            } else {
                batch
            };
            if batch.num_rows() == 0 {
                continue;
            }
            let batch = normalize_batch(batch);
            offsets.push(total);
            total += batch.num_rows();
            batches.push(batch);
        }
        let columns = schema
            .fields()
            .iter()
            .map(|f| Column {
                name: f.name().clone(),
                type_name: duck_type_name(f.data_type()),
                numeric: is_numeric(f.data_type()),
            })
            .collect();
        Ok(QueryResult {
            columns,
            batches,
            offsets,
            rows: total,
            truncated,
            elapsed: started.elapsed(),
        })
    }

    /// Interrupt the query in flight. Quack has no remote cancel yet, so the
    /// server may keep computing; callers should abandon the pending result.
    pub fn cancel(&self) {
        if let Some(handle) = self.inner.running.lock().unwrap().take() {
            handle.interrupt();
        }
    }

    /// Small helper for metadata lookups: everything comes back as strings.
    pub fn meta_rows(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
        let wrapped = format!("SELECT COLUMNS(*)::VARCHAR FROM ({})", self.wrap(sql));
        let conn = self.inner.meta.lock().unwrap();
        let mut stmt = conn.prepare(&wrapped).map_err(|e| self.clean_error(e))?;
        let mut rows = stmt.query([]).map_err(|e| self.clean_error(e))?;
        let mut out = Vec::new();
        while let Some(row) = rows.next()? {
            let ncols = row.as_ref().column_count();
            let mut r = Vec::with_capacity(ncols);
            for i in 0..ncols {
                r.push(row.get::<_, Option<String>>(i)?);
            }
            out.push(r);
        }
        Ok(out)
    }

    /// The table's best declared row key: its primary key, else its first
    /// UNIQUE constraint. `None` if it declares neither (DuckLake tables never do).
    pub fn declared_key(&self, rel: &Relation) -> Result<Option<(KeyKind, Vec<String>)>> {
        let rows = self.meta_rows(&format!(
            "SELECT constraint_type, constraint_index, col FROM ( \
               SELECT constraint_type, constraint_index, \
                      unnest(constraint_column_names) AS col, \
                      generate_subscripts(constraint_column_names, 1) AS pos \
               FROM duckdb_constraints() \
               WHERE constraint_type IN ('PRIMARY KEY', 'UNIQUE') \
                 AND database_name = {} AND schema_name = {} AND table_name = {}) \
             ORDER BY constraint_type <> 'PRIMARY KEY', constraint_index, pos",
            sql_literal(&rel.database),
            sql_literal(&rel.schema),
            sql_literal(&rel.name),
        ))?;
        let get = |r: &[Option<String>], i: usize| r.get(i).cloned().flatten().unwrap_or_default();
        let Some(first) = rows.first() else {
            return Ok(None);
        };
        let (kind, index) = (get(first, 0), get(first, 1));
        let columns = rows
            .iter()
            .take_while(|r| get(r, 0) == kind && get(r, 1) == index)
            .map(|r| get(r, 2))
            .collect();
        let kind = if kind == "PRIMARY KEY" {
            KeyKind::Primary
        } else {
            KeyKind::Unique
        };
        Ok(Some((kind, columns)))
    }

    /// Run statements in one transaction: all of them apply, or none do.
    pub fn execute_transaction(&self, statements: &[String]) -> Result<()> {
        let conn = self.inner.base.lock().unwrap().try_clone()?;
        let body = statements.join(";\n");
        if self.is_local() {
            conn.execute_batch("BEGIN TRANSACTION")?;
            if let Err(e) = conn.execute_batch(&body) {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(self.clean_error(e));
            }
            conn.execute_batch("COMMIT")
                .map_err(|e| self.clean_error(e))
        } else {
            let script = format!("BEGIN TRANSACTION;\n{body};\nCOMMIT;");
            let result = conn
                .prepare(&self.wrap(&script))
                .and_then(|mut stmt| stmt.query([]).map(|_| ()))
                .map_err(|e| self.clean_error(e));
            if result.is_err() {
                // In case the server kept the failed transaction open.
                let _ = conn
                    .prepare(&self.wrap("ROLLBACK"))
                    .and_then(|mut stmt| stmt.query([]).map(|_| ()));
            }
            result
        }
    }

    /// Load a CSV file on this machine into a new table. The file is parsed
    /// locally (DuckDB's sniffer picks the column types), then written in
    /// chunks so `progress` can follow along. Each import has its own
    /// connection, so several can run at once. A failed or cancelled import
    /// leaves no table behind.
    pub fn import_csv(
        &self,
        path: &Path,
        target: &ImportTarget,
        progress: &ImportProgress,
    ) -> Result<u64> {
        let default_db = self
            .meta_rows("SELECT current_database()")?
            .into_iter()
            .next()
            .and_then(|r| r.into_iter().next().flatten())
            .ok_or_else(|| anyhow!("couldn't find the default database"))?;
        let database = target.database.clone().unwrap_or_else(|| default_db.clone());
        let exists = self.meta_rows(&format!(
            "SELECT 1 FROM ( \
               SELECT database_name, schema_name, table_name AS name FROM duckdb_tables() \
               UNION ALL SELECT database_name, schema_name, view_name FROM duckdb_views()) \
             WHERE lower(database_name) = lower({}) AND lower(schema_name) = lower({}) \
               AND lower(name) = lower({})",
            sql_literal(&database),
            sql_literal(&target.schema),
            sql_literal(&target.table),
        ))?;
        if !exists.is_empty() {
            bail!("{}.{} already exists", target.schema, target.table);
        }
        let final_name = format!(
            "{}.{}.{}",
            quote_ident(&database),
            quote_ident(&target.schema),
            quote_ident(&target.table)
        );
        let tmp = format!("__duckplus_import_{}", uuid::Uuid::new_v4().simple());

        // `dest` is where chunks go, as `conn` sees it; `finish` is a server
        // script that moves them into place when that isn't `final_name`.
        let (conn, dest, finish) = match &self.inner.remote {
            None => {
                let conn = self.inner.base.lock().unwrap().try_clone()?;
                (conn, final_name.clone(), None)
            }
            Some(remote) => {
                // Quack only reaches the server's default database this way.
                let conn = Connection::open_in_memory()?;
                let mut attach = format!(
                    "LOAD quack; ATTACH {} AS remote (TOKEN {}",
                    sql_literal(&remote.endpoint),
                    sql_literal(&remote.token)
                );
                if remote.tls == TlsMode::Disabled {
                    attach.push_str(", DISABLE_SSL true");
                }
                attach.push(')');
                conn.execute_batch(&attach).map_err(|e| self.clean_error(e))?;
                if database.eq_ignore_ascii_case(&default_db) {
                    let dest = format!(
                        "remote.{}.{}",
                        quote_ident(&target.schema),
                        quote_ident(&target.table)
                    );
                    (conn, dest, None)
                } else {
                    let staged = format!("{}.main.{}", quote_ident(&default_db), quote_ident(&tmp));
                    let finish = format!(
                        "CREATE TABLE {final_name} AS FROM {staged}; DROP TABLE {staged};"
                    );
                    (conn, format!("remote.main.{}", quote_ident(&tmp)), Some(finish))
                }
            }
        };

        let stage = quote_ident(&tmp);
        conn.execute_batch(&format!(
            "CREATE TEMP TABLE {stage} AS SELECT * FROM read_csv({})",
            sql_literal(&path.to_string_lossy())
        ))
        .map_err(|e| self.clean_error(e))?;
        let total: u64 = conn
            .query_row(&format!("SELECT count(*) FROM {stage}"), [], |r| r.get(0))
            .map_err(|e| self.clean_error(e))?;
        progress.total.store(total.max(1), Relaxed);

        let mut created = false;
        let result = (|| -> Result<()> {
            conn.execute_batch(&format!("CREATE TABLE {dest} AS FROM {stage} LIMIT 0"))
                .map_err(|e| self.clean_error(e))?;
            created = true;
            let chunk = (total / 100).clamp(10_000, 250_000);
            let mut at = 0;
            while at < total {
                if progress.cancelled.load(Relaxed) {
                    bail!("Cancelled");
                }
                let end = (at + chunk).min(total);
                conn.execute_batch(&format!(
                    "INSERT INTO {dest} SELECT * FROM {stage} WHERE rowid >= {at} AND rowid < {end}"
                ))
                .map_err(|e| self.clean_error(e))?;
                at = end;
                progress.done.store(at, Relaxed);
            }
            if progress.cancelled.load(Relaxed) {
                bail!("Cancelled");
            }
            if let Some(script) = &finish {
                conn.prepare(&self.wrap(script))
                    .and_then(|mut stmt| stmt.query([]).map(|_| ()))
                    .map_err(|e| self.clean_error(e))?;
            }
            Ok(())
        })();
        if result.is_err() && created {
            let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS {dest}"));
        }
        let _ = conn.execute_batch(&format!("DROP TABLE IF EXISTS {stage}"));
        result.map(|()| {
            progress.done.store(total.max(1), Relaxed);
            total
        })
    }

    /// Load every non-internal relation on the server in a single round trip.
    pub fn catalog(&self) -> Result<Catalog> {
        let rows = self.meta_rows(
            "SELECT database_name, schema_name, table_name, 'table' AS kind, estimated_size, column_count \
               FROM duckdb_tables() WHERE NOT internal \
             UNION ALL \
             SELECT database_name, schema_name, view_name, 'view', NULL, column_count \
               FROM duckdb_views() WHERE NOT internal \
             ORDER BY 1, 2, 4, 3",
        )?;
        let mut catalog = Catalog::default();
        for r in rows {
            let get = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
            catalog.relations.push(Relation {
                database: get(0),
                schema: get(1),
                name: get(2),
                is_view: get(3) == "view",
                estimated_rows: r.get(4).cloned().flatten().and_then(|s| s.parse().ok()),
                columns: r.get(5).cloned().flatten().and_then(|s| s.parse().ok()),
            });
        }
        // Separately, so databases without any tables still show up.
        catalog.databases = self
            .meta_rows(
                "SELECT database_name FROM duckdb_databases() \
                 WHERE NOT internal ORDER BY database_name",
            )?
            .into_iter()
            .filter_map(|r| r.into_iter().next().flatten())
            .collect();
        // Autocomplete vocabulary: columns, functions and reserved words, in
        // one round trip. Optional, so a server that can't answer still
        // gets a browsable schema.
        if let Ok(rows) = self.meta_rows(
            "SELECT 'c' AS kind, database_name, schema_name, table_name, column_name, \
                    data_type, column_index \
               FROM duckdb_columns() WHERE NOT internal \
             UNION ALL \
             SELECT DISTINCT 'f', NULL, NULL, NULL, function_name, function_type, NULL \
               FROM duckdb_functions() \
              WHERE function_type <> 'pragma' AND NOT starts_with(function_name, '__') \
             UNION ALL \
             SELECT 'k', NULL, NULL, NULL, keyword_name, NULL, NULL \
               FROM duckdb_keywords() WHERE keyword_category = 'reserved' \
             ORDER BY 1, 2, 3, 4, 7",
        ) {
            for r in rows {
                let get = |i: usize| r.get(i).cloned().flatten().unwrap_or_default();
                match get(0).as_str() {
                    "c" => catalog.columns.push(TableColumn {
                        database: get(1),
                        schema: get(2),
                        table: get(3),
                        name: get(4),
                        data_type: get(5),
                    }),
                    "f" => catalog.functions.push((get(4), get(5))),
                    _ => catalog.reserved.push(get(4)),
                }
            }
        }
        if let Some(row) = self
            .meta_rows("SELECT current_database(), current_schema()")?
            .into_iter()
            .next()
        {
            catalog.current_database = row.first().cloned().flatten();
            catalog.current_schema = row.get(1).cloned().flatten();
        }
        Ok(catalog)
    }
}

/// DuckDB ships TIMESTAMPTZ as UTC tagged with the named zone "UTC", which
/// Arrow can't format without chrono-tz. Retag as the equivalent fixed offset.
fn normalize_batch(batch: RecordBatch) -> RecordBatch {
    let needs =
        |t: &DataType| matches!(t, DataType::Timestamp(_, Some(tz)) if !tz.starts_with(['+', '-']));
    if !batch.schema().fields().iter().any(|f| needs(f.data_type())) {
        return batch;
    }
    let mut fields = Vec::new();
    let mut columns = Vec::new();
    for (field, col) in batch.schema().fields().iter().zip(batch.columns()) {
        match field.data_type() {
            DataType::Timestamp(unit, Some(_)) if needs(field.data_type()) => {
                let target = DataType::Timestamp(*unit, Some("+00:00".into()));
                match cast(col, &target) {
                    Ok(c) => {
                        fields.push(field.as_ref().clone().with_data_type(target));
                        columns.push(c);
                    }
                    Err(_) => {
                        fields.push(field.as_ref().clone());
                        columns.push(col.clone());
                    }
                }
            }
            _ => {
                fields.push(field.as_ref().clone());
                columns.push(col.clone());
            }
        }
    }
    RecordBatch::try_new(Arc::new(Schema::new(fields)), columns).unwrap_or(batch)
}

/// Live progress of one CSV import, shared with the UI.
#[derive(Default)]
pub struct ImportProgress {
    /// Rows written so far.
    pub done: AtomicU64,
    /// Rows in the file; 0 while it's still being read.
    pub total: AtomicU64,
    /// Set to stop the import before its next chunk.
    pub cancelled: AtomicBool,
}

impl ImportProgress {
    /// 0.0–1.0, or `None` while the file is still being read.
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total.load(Relaxed);
        (total > 0).then(|| self.done.load(Relaxed) as f32 / total as f32)
    }
}

/// Where an imported file lands; `database: None` is the default database.
#[derive(Debug, Clone)]
pub struct ImportTarget {
    pub database: Option<String>,
    pub schema: String,
    pub table: String,
}

impl QuackClient {
    fn clean_error(&self, e: duckdb::Error) -> anyhow::Error {
        if self.is_local() {
            // Local errors don't carry a token, and their LINE context is useful.
            return anyhow!(e.to_string());
        }
        clean_quack_error(e)
    }
}

/// Split a script into statements on `;`, ignoring semicolons inside quotes,
/// dollar-quoted strings and comments. Empty statements are dropped.
fn split_statements(sql: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = sql.chars().peekable();
    let flush = |current: &mut String, out: &mut Vec<String>| {
        let has_code = strip_comments(current).trim().len() > 0;
        if has_code {
            out.push(current.trim().to_string());
        }
        current.clear();
    };
    while let Some(c) = chars.next() {
        match c {
            '\'' | '"' => {
                current.push(c);
                while let Some(n) = chars.next() {
                    current.push(n);
                    if n == c {
                        // A doubled quote is an escape, not the end.
                        if chars.peek() == Some(&c) {
                            current.push(chars.next().unwrap());
                        } else {
                            break;
                        }
                    }
                }
            }
            '$' if chars.peek() == Some(&'$') => {
                current.push_str("$$");
                chars.next();
                while let Some(n) = chars.next() {
                    current.push(n);
                    if n == '$' && chars.peek() == Some(&'$') {
                        current.push(chars.next().unwrap());
                        break;
                    }
                }
            }
            '-' if chars.peek() == Some(&'-') => {
                current.push(c);
                for n in chars.by_ref() {
                    current.push(n);
                    if n == '\n' {
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                current.push(c);
                current.push(chars.next().unwrap());
                let mut prev = ' ';
                for n in chars.by_ref() {
                    current.push(n);
                    if prev == '*' && n == '/' {
                        break;
                    }
                    prev = n;
                }
            }
            ';' => flush(&mut current, &mut out),
            _ => current.push(c),
        }
    }
    flush(&mut current, &mut out);
    out
}

/// Drop `--` line comments and `/* */` block comments (quote-unaware; only
/// used to tell whether a split-off piece has any code in it).
fn strip_comments(sql: &str) -> String {
    let mut out = String::new();
    let mut rest = sql;
    while !rest.is_empty() {
        if let Some(r) = rest.strip_prefix("--") {
            rest = r.find('\n').map_or("", |i| &r[i..]);
        } else if let Some(r) = rest.strip_prefix("/*") {
            rest = r.find("*/").map_or("", |i| &r[i + 2..]);
        } else {
            let c = rest.chars().next().unwrap();
            out.push(c);
            rest = &rest[c.len_utf8()..];
        }
    }
    out
}

fn clean_quack_error(e: duckdb::Error) -> anyhow::Error {
    // quack_query errors echo the whole wrapped statement ("LINE 1: SELECT * FROM quack_query(...")
    // which leaks the token into the UI; keep only the meaningful first part.
    let msg = e.to_string();
    let msg = msg.split("\n\nLINE ").next().unwrap_or(&msg).trim();
    let msg = msg
        .strip_prefix("IO Error: Failed to send message: ")
        .unwrap_or(msg);
    anyhow!(msg.to_string())
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: String,
    pub type_name: String,
    pub numeric: bool,
}

pub struct QueryResult {
    pub columns: Vec<Column>,
    batches: Vec<RecordBatch>,
    offsets: Vec<usize>,
    pub rows: usize,
    pub truncated: bool,
    pub elapsed: Duration,
}

impl QueryResult {
    fn locate(&self, row: usize) -> Option<(&RecordBatch, usize)> {
        if row >= self.rows {
            return None;
        }
        let ix = self.offsets.partition_point(|&o| o <= row) - 1;
        Some((&self.batches[ix], row - self.offsets[ix]))
    }

    /// Format one cell. `None` means SQL NULL.
    pub fn cell(&self, row: usize, col: usize) -> Option<String> {
        let (batch, local) = self.locate(row)?;
        let array = batch.column(col);
        if array.is_null(local) {
            return None;
        }
        let opts = FormatOptions::default()
            .with_display_error(true)
            .with_timestamp_format(Some("%Y-%m-%d %H:%M:%S%.f"))
            .with_timestamp_tz_format(Some("%Y-%m-%d %H:%M:%S%.f%:z"));
        let fmt = ArrayFormatter::try_new(array.as_ref(), &opts).ok()?;
        Some(fmt.value(local).to_string())
    }

    /// Row order sorted by one column (NULLs last), for sorting in place.
    pub fn sorted_order(&self, col: usize, descending: bool) -> Vec<usize> {
        let arrays: Vec<&dyn Array> = self
            .batches
            .iter()
            .map(|b| b.column(col).as_ref())
            .collect();
        let options = SortOptions {
            descending,
            nulls_first: false,
        };
        concat(&arrays)
            .and_then(|all| sort_to_indices(&all, Some(options), None))
            .map(|ix| ix.values().iter().map(|&i| i as usize).collect())
            .unwrap_or_else(|_| (0..self.rows).collect())
    }

    /// CSV (RFC 4180 quoting) of the given rows and columns, with a header
    /// line. NULL becomes an empty field.
    pub fn to_csv(&self, rows: impl IntoIterator<Item = usize>, cols: &[usize]) -> String {
        fn field(s: &str) -> String {
            if s.contains([',', '"', '\n', '\r']) {
                format!("\"{}\"", s.replace('"', "\"\""))
            } else {
                s.to_string()
            }
        }
        let mut out = cols
            .iter()
            .map(|&c| field(&self.columns[c].name))
            .collect::<Vec<_>>()
            .join(",");
        for r in rows.into_iter().filter(|&r| r < self.rows) {
            out.push('\n');
            let line = cols
                .iter()
                .map(|&c| self.cell(r, c).map(|v| field(&v)).unwrap_or_default())
                .collect::<Vec<_>>()
                .join(",");
            out.push_str(&line);
        }
        out
    }

    /// Tab-separated dump for the clipboard.
    pub fn to_tsv(&self) -> String {
        let mut out = self
            .columns
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join("\t");
        for r in 0..self.rows {
            out.push('\n');
            let line = (0..self.columns.len())
                .map(|c| self.cell(r, c).unwrap_or_else(|| "NULL".into()))
                .collect::<Vec<_>>()
                .join("\t");
            out.push_str(&line);
        }
        out
    }
}

/// How a table's rows are identified for editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyKind {
    /// A declared PRIMARY KEY.
    Primary,
    /// A declared UNIQUE constraint.
    Unique,
    /// A column the user (or a guess like `id`) picked; uniqueness is only
    /// enforced per save, by checking each update hits exactly one row.
    Column,
}

#[derive(Debug, Clone, Default)]
pub struct Catalog {
    pub relations: Vec<Relation>,
    pub databases: Vec<String>,
    /// Where unqualified names resolve (without a focused database).
    pub current_database: Option<String>,
    pub current_schema: Option<String>,
    /// Every relation's columns, grouped by relation in column order.
    pub columns: Vec<TableColumn>,
    /// (name, function_type) for each function and macro.
    pub functions: Vec<(String, String)>,
    /// Reserved keywords, which must be quoted to be used as names.
    pub reserved: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TableColumn {
    pub database: String,
    pub schema: String,
    pub table: String,
    pub name: String,
    pub data_type: String,
}

#[derive(Debug, Clone)]
pub struct Relation {
    pub database: String,
    pub schema: String,
    pub name: String,
    pub is_view: bool,
    pub estimated_rows: Option<u64>,
    pub columns: Option<u64>,
}

impl Relation {
    pub fn qualified(&self) -> String {
        format!(
            "{}.{}.{}",
            quote_ident(&self.database),
            quote_ident(&self.schema),
            quote_ident(&self.name)
        )
    }
}

fn is_numeric(t: &DataType) -> bool {
    use DataType::*;
    matches!(
        t,
        Int8 | Int16
            | Int32
            | Int64
            | UInt8
            | UInt16
            | UInt32
            | UInt64
            | Float16
            | Float32
            | Float64
            | Decimal128(..)
            | Decimal256(..)
    )
}

/// Map the Arrow type DuckDB hands us back to the DuckDB type name users know.
fn duck_type_name(t: &DataType) -> String {
    use DataType::*;
    match t {
        Boolean => "BOOLEAN".into(),
        Int8 => "TINYINT".into(),
        Int16 => "SMALLINT".into(),
        Int32 => "INTEGER".into(),
        Int64 => "BIGINT".into(),
        UInt8 => "UTINYINT".into(),
        UInt16 => "USMALLINT".into(),
        UInt32 => "UINTEGER".into(),
        UInt64 => "UBIGINT".into(),
        Float16 | Float32 => "FLOAT".into(),
        Float64 => "DOUBLE".into(),
        Decimal128(p, s) | Decimal256(p, s) => format!("DECIMAL({p},{s})"),
        Utf8 | LargeUtf8 | Utf8View => "VARCHAR".into(),
        Binary | LargeBinary | BinaryView => "BLOB".into(),
        FixedSizeBinary(16) => "UUID".into(),
        FixedSizeBinary(_) => "BLOB".into(),
        Date32 | Date64 => "DATE".into(),
        Time32(_) | Time64(_) => "TIME".into(),
        Timestamp(_, Some(_)) => "TIMESTAMPTZ".into(),
        Timestamp(_, None) => "TIMESTAMP".into(),
        Interval(_) | Duration(_) => "INTERVAL".into(),
        List(f) | LargeList(f) | ListView(f) | LargeListView(f) => {
            format!("{}[]", duck_type_name(f.data_type()))
        }
        FixedSizeList(f, n) => format!("{}[{n}]", duck_type_name(f.data_type())),
        Struct(_) => "STRUCT".into(),
        Map(..) => "MAP".into(),
        Union(..) => "UNION".into(),
        Dictionary(_, v) => duck_type_name(v),
        Null => "NULL".into(),
        other => format!("{other}").to_uppercase(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints() {
        assert_eq!(normalize_endpoint("localhost"), "quack:localhost");
        assert_eq!(normalize_endpoint("quack:db.io:9494"), "quack:db.io:9494");
        assert_eq!(normalize_endpoint("https://db.io/"), "quack:db.io");
        assert_eq!(
            normalize_endpoint("  http://db.io:1234/quack "),
            "quack:db.io:1234"
        );
    }

    /// Runs against a live server: DUCKPLUS_TEST_TOKEN=secret cargo test -- --ignored
    #[test]
    #[ignore]
    fn live_roundtrip() {
        let token = std::env::var("DUCKPLUS_TEST_TOKEN").unwrap();
        let endpoint =
            std::env::var("DUCKPLUS_TEST_ENDPOINT").unwrap_or_else(|_| "localhost".into());
        let (client, info) = QuackClient::connect(&endpoint, &token, TlsMode::Auto).unwrap();
        assert!(info.version.starts_with('v'));
        let r = client
            .run(
                "SELECT range AS i, 'it''s' AS s, [1,2] AS l FROM range(5000)",
                1200,
                None,
            )
            .unwrap();
        assert_eq!(r.rows, 1200);
        assert!(r.truncated);
        assert_eq!(r.cell(1100, 0).as_deref(), Some("1100"));
        assert_eq!(r.cell(0, 1).as_deref(), Some("it's"));
        assert_eq!(r.columns[2].type_name, "INTEGER[]");
        let ts = client
            .run("SELECT TIMESTAMPTZ '2024-01-01 10:00:00+00' AS t", 1, None)
            .unwrap();
        assert_eq!(ts.columns[0].type_name, "TIMESTAMPTZ");
        assert_eq!(ts.cell(0, 0).as_deref(), Some("2024-01-01 10:00:00+00:00"));
        let cat = client.catalog().unwrap();
        assert!(!cat.relations.is_empty());
        // The autocomplete vocabulary loads over Quack too.
        assert!(!cat.columns.is_empty());
        assert!(cat.functions.iter().any(|(f, _)| f == "count"));
        assert!(cat.reserved.iter().any(|k| k == "select"));
        // A slow query in flight must not block the next one.
        let c2 = client.clone();
        let slow = std::thread::spawn(move || {
            c2.run(
                "SELECT sum(a.range * b.range) FROM range(40000) a, range(40000) b",
                10,
                None,
            )
        });
        std::thread::sleep(Duration::from_millis(200));
        client.cancel();
        let started = Instant::now();
        let quick = client.run("SELECT 42", 10, None).unwrap();
        assert_eq!(quick.cell(0, 0).as_deref(), Some("42"));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        let _ = slow.join();

        let err = client.run("SELEC 1", 10, None).err().unwrap().to_string();
        assert!(!err.contains(&token), "{err}");
    }
}

#[cfg(test)]
mod split_tests {
    use super::split_statements;

    #[test]
    fn splits_on_top_level_semicolons() {
        let sql = "CREATE TABLE t(a TEXT); -- note; here\nINSERT INTO t VALUES ('a;b'), (\"x\"\";\");\n/* ; */ SELECT $$;$$;\n-- trailing";
        assert_eq!(
            split_statements(sql),
            vec![
                "CREATE TABLE t(a TEXT)",
                "-- note; here\nINSERT INTO t VALUES ('a;b'), (\"x\"\";\")",
                "/* ; */ SELECT $$;$$",
            ]
        );
    }

    #[test]
    fn single_statement_without_semicolon() {
        assert_eq!(split_statements("SELECT 1"), vec!["SELECT 1"]);
        assert!(split_statements("  -- nothing\n").is_empty());
    }
}

#[cfg(test)]
mod local_tests {
    use super::QuackClient;

    #[test]
    fn local_file_roundtrip() {
        let path = std::env::temp_dir().join(format!("duckplus-{}.duckdb", uuid::Uuid::new_v4()));
        duckdb::Connection::open(&path).unwrap();

        let (client, info) = QuackClient::connect_local(&path, false).unwrap();
        assert!(client.is_local());
        assert!(!info.version.is_empty());

        // Scripts run every statement and show the last one's result.
        let r = client
            .run("CREATE TABLE t AS SELECT 1 AS a; INSERT INTO t VALUES (2); SELECT * FROM t ORDER BY a", 100, None)
            .unwrap();
        assert_eq!(r.rows, 2);
        assert_eq!(client.catalog().unwrap().relations[0].name, "t");

        // A second open of the same file shares the database instead of
        // tripping over its lock.
        let (again, _) = QuackClient::connect_local(&path, false).unwrap();
        assert_eq!(
            again.run("SELECT count(*) FROM t", 10, None).unwrap().rows,
            1
        );

        drop((client, again));
        let (ro, _) = QuackClient::connect_local(&path, true).unwrap();
        assert!(ro.run("INSERT INTO t VALUES (3)", 10, None).is_err());
        drop(ro);
        let _ = std::fs::remove_file(&path);
    }
}

#[cfg(test)]
mod editing_tests {
    use super::{QuackClient, Relation};

    #[test]
    fn primary_key_transactions_sort_and_focus() {
        let path = std::env::temp_dir().join(format!("duckplus-{}.duckdb", uuid::Uuid::new_v4()));
        let (client, _) = QuackClient::connect_local(&path, false).unwrap_or_else(|_| {
            duckdb::Connection::open(&path).unwrap();
            QuackClient::connect_local(&path, false).unwrap()
        });
        client
            .run(
                "CREATE TABLE users(id INT PRIMARY KEY, name TEXT, age INT); \
                 INSERT INTO users VALUES (1, 'ada', 36), (2, 'grace', NULL), (3, 'alan', 41); \
                 SELECT 1",
                10,
                None,
            )
            .unwrap();

        let catalog = client.catalog().unwrap();
        let rel: Relation = catalog
            .relations
            .into_iter()
            .find(|r| r.name == "users")
            .unwrap();
        assert_eq!(
            client.declared_key(&rel).unwrap(),
            Some((super::KeyKind::Primary, vec!["id".to_string()]))
        );
        assert!(!catalog.databases.is_empty());

        // A failing statement rolls back the ones before it.
        let bad = client.execute_transaction(&[
            format!("UPDATE {} SET name = 'ADA' WHERE id = '1'", rel.qualified()),
            format!(
                "UPDATE {} SET age = 'not a number' WHERE id = '2'",
                rel.qualified()
            ),
        ]);
        assert!(bad.is_err());
        let r = client
            .run("SELECT name FROM users WHERE id = 1", 10, None)
            .unwrap();
        assert_eq!(r.cell(0, 0).as_deref(), Some("ada"));

        client
            .execute_transaction(&[
                format!(
                    "UPDATE {} SET name = 'ADA', age = '37' WHERE id = '1'",
                    rel.qualified()
                ),
                format!("UPDATE {} SET age = NULL WHERE id = '3'", rel.qualified()),
            ])
            .unwrap();
        let r = client
            .run("SELECT name, age FROM users ORDER BY id", 10, None)
            .unwrap();
        assert_eq!(r.cell(0, 0).as_deref(), Some("ADA"));
        assert_eq!(r.cell(0, 1).as_deref(), Some("37"));
        assert_eq!(r.cell(2, 1), None);

        // No declared key; a guessed `id` with a duplicate. The per-row
        // guard (as the workspace builds it) must abort the whole save.
        client
            .run(
                "CREATE TABLE nokey(id INT, name TEXT); \
                 INSERT INTO nokey VALUES (1, 'a'), (5, 'b'), (5, 'c'); SELECT 1",
                10,
                None,
            )
            .unwrap();
        let catalog = client.catalog().unwrap();
        let nokey = catalog
            .relations
            .iter()
            .find(|r| r.name == "nokey")
            .unwrap();
        assert_eq!(client.declared_key(nokey).unwrap(), None);
        let guarded = |filter: &str, set: &str| {
            vec![
                format!(
                    "SELECT CASE WHEN count(*) <> 1 THEN error({} || count(*) || ' rows, nothing was saved') END FROM {} WHERE {filter}",
                    super::sql_literal(&format!("Expected 1 row where {filter}, found ")),
                    nokey.qualified()
                ),
                format!("UPDATE {} SET {set} WHERE {filter}", nokey.qualified()),
            ]
        };
        let mut statements = guarded("\"id\" = '1'", "name = 'A'");
        statements.extend(guarded("\"id\" = '5'", "name = 'X'"));
        let err = client
            .execute_transaction(&statements)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Expected 1 row"), "{err}");
        let r = client
            .run("SELECT name FROM nokey ORDER BY id, name", 10, None)
            .unwrap();
        assert_eq!(r.cell(0, 0).as_deref(), Some("a"), "rolled back");
        assert!(
            client
                .execute_transaction(&guarded("\"id\" = '1'", "name = 'A'"))
                .is_ok()
        );

        // In-place sort: descending by name, NULLs last.
        let r = client.run("SELECT name, age FROM users", 10, None).unwrap();
        let order = r.sorted_order(1, true);
        assert_eq!(r.cell(order[0], 1).as_deref(), Some("37"));
        assert_eq!(r.cell(order[2], 1), None);

        // Focused database: unqualified names resolve against it.
        let db = rel.database.clone();
        let r = client
            .run("SELECT count(*) FROM main.users", 10, Some(&db))
            .unwrap();
        assert_eq!(r.cell(0, 0).as_deref(), Some("3"));

        drop(client);
        let _ = std::fs::remove_file(&path);
    }
}

/// Against a local Quack server:
/// DUCKPLUS_QUACK=quack:localhost:9595 DUCKPLUS_TOKEN=sekret cargo test quack_editing -- --ignored --nocapture
#[cfg(test)]
mod quack_editing_tests {
    use super::{QuackClient, TlsMode};

    #[test]
    #[ignore]
    fn quack_editing() {
        let endpoint = std::env::var("DUCKPLUS_QUACK").unwrap();
        let token = std::env::var("DUCKPLUS_TOKEN").unwrap();
        let (client, _) = QuackClient::connect(&endpoint, &token, TlsMode::Auto).unwrap();
        let catalog = client.catalog().unwrap();
        println!("databases: {:?}", catalog.databases);
        for rel in &catalog.relations {
            println!(
                "{} -> key {:?}",
                rel.qualified(),
                client.declared_key(rel).map_err(|e| e.to_string())
            );
        }
        let rel = catalog
            .relations
            .iter()
            .find(|r| r.name == "people")
            .unwrap();
        let tx = client.execute_transaction(&[format!(
            "UPDATE {} SET city = 'paris' WHERE \"id\" = '1'",
            rel.qualified()
        )]);
        println!("transaction: {:?}", tx.as_ref().map_err(|e| e.to_string()));
        let r = client.run(
            "SELECT city FROM people WHERE id = 1",
            10,
            Some(&rel.database),
        );
        let nokey = catalog
            .relations
            .iter()
            .find(|r| r.name == "nokey")
            .unwrap();
        println!(
            "nokey declared key: {:?}",
            client.declared_key(nokey).map_err(|e| e.to_string())
        );
        let guarded = |filter: &str, set: &str| {
            vec![
                format!(
                    "SELECT CASE WHEN count(*) <> 1 THEN error({} || count(*) || ' rows, nothing was saved') END FROM {} WHERE {filter}",
                    super::sql_literal(&format!("Expected 1 row where {filter}, found ")),
                    nokey.qualified()
                ),
                format!("UPDATE {} SET {set} WHERE {filter}", nokey.qualified()),
            ]
        };
        let mut statements = guarded("\"id\" = '1'", "name = 'A'");
        statements.extend(guarded("\"id\" = '5'", "name = 'X'"));
        println!(
            "guarded dup save: {:?}",
            client
                .execute_transaction(&statements)
                .map_err(|e| e.to_string())
        );
        let after = client
            .run("SELECT name FROM nokey ORDER BY id, name", 10, None)
            .unwrap();
        println!(
            "nokey after failed save: {:?}",
            (0..3).map(|i| after.cell(i, 0)).collect::<Vec<_>>()
        );
        println!(
            "guarded ok save: {:?}",
            client
                .execute_transaction(&guarded("\"id\" = '1'", "name = 'A'"))
                .map_err(|e| e.to_string())
        );
        let after = client
            .run("SELECT name FROM nokey ORDER BY id, name", 10, None)
            .unwrap();
        println!(
            "nokey after good save: {:?}",
            (0..3).map(|i| after.cell(i, 0)).collect::<Vec<_>>()
        );
        println!(
            "after (with USE): {:?}",
            r.map(|r| r.cell(0, 0)).map_err(|e| e.to_string())
        );
    }
}

/// Needs a Quack server with a `lake` database attached (see
/// `quack_editing`) and `DUCKPLUS_CSV` pointing at a CSV file.
#[cfg(test)]
mod quack_import_tests {
    use super::{ImportProgress, ImportTarget, QuackClient, TlsMode};
    use std::sync::atomic::Ordering::Relaxed;

    fn target(database: Option<&str>, table: &str) -> ImportTarget {
        ImportTarget {
            database: database.map(str::to_string),
            schema: "main".into(),
            table: table.into(),
        }
    }

    #[test]
    fn local_import() {
        let dir = std::env::temp_dir().join(format!("duckplus-import-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let csv = dir.join("people.csv");
        std::fs::write(&csv, "id,name,born\n1,ada,1815-12-10\n2,alan,1912-06-23\n").unwrap();
        let db = dir.join("x.duckdb");
        duckdb::Connection::open(&db).unwrap();
        let (client, _) = QuackClient::connect_local(&db, false).unwrap();
        client.run("CREATE SCHEMA staging", 1, None).unwrap();
        let t = ImportTarget { database: None, schema: "staging".into(), table: "people".into() };
        let progress = ImportProgress::default();
        assert_eq!(client.import_csv(&csv, &t, &progress).unwrap(), 2);
        assert_eq!(progress.fraction(), Some(1.0));
        let rows = client
            .meta_rows("SELECT typeof(born), count(*) FROM staging.people GROUP BY ALL")
            .unwrap();
        assert_eq!(rows, vec![vec![Some("DATE".into()), Some("2".into())]]);
        drop(client);
        std::fs::remove_dir_all(dir).ok();
    }

    #[test]
    #[ignore]
    fn quack_import() {
        let endpoint = std::env::var("DUCKPLUS_QUACK").unwrap();
        let token = std::env::var("DUCKPLUS_TOKEN").unwrap();
        let csv = std::path::PathBuf::from(std::env::var("DUCKPLUS_CSV").unwrap());
        let (client, _) = QuackClient::connect(&endpoint, &token, TlsMode::Auto).unwrap();
        let suffix = uuid::Uuid::new_v4().simple().to_string()[..8].to_string();
        let (a, b) = (format!("imp_a_{suffix}"), format!("imp_b_{suffix}"));

        // Two files at once, one into another database.
        std::thread::scope(|s| {
            let jobs = [(None, a.as_str()), (Some("lake"), b.as_str())].map(|(db, name)| {
                let (client, csv) = (client.clone(), csv.clone());
                s.spawn(move || {
                    let progress = ImportProgress::default();
                    let rows = client.import_csv(&csv, &target(db, name), &progress).unwrap();
                    assert_eq!(progress.fraction(), Some(1.0));
                    rows
                })
            });
            for job in jobs {
                assert!(job.join().unwrap() > 0);
            }
        });
        let count = |sql: &str| client.meta_rows(sql).unwrap()[0][0].clone().unwrap();
        println!("default: {}", count(&format!("SELECT count(*) FROM main.{a}")));
        println!("lake: {}", count(&format!("SELECT count(*) FROM lake.main.{b}")));
        assert_eq!(
            count(&format!("SELECT count(*) FROM duckdb_tables() WHERE table_name LIKE '__duckplus_import_%'")),
            "0"
        );

        let err = client
            .import_csv(&csv, &target(None, &a), &ImportProgress::default())
            .unwrap_err();
        assert!(err.to_string().contains("already exists"), "{err}");

        let cancelled = ImportProgress::default();
        cancelled.cancelled.store(true, Relaxed);
        let c = format!("imp_c_{suffix}");
        assert!(client.import_csv(&csv, &target(None, &c), &cancelled).is_err());
        assert_eq!(
            count(&format!("SELECT count(*) FROM duckdb_tables() WHERE table_name = '{c}'")),
            "0"
        );
    }
}
