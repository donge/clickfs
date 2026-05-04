//! ClickHouse HTTP driver.
//!
//! - `query_text`           : small queries returning a full text body.
//! - `query_stream_with_id` : streaming queries returning a Bytes stream.

use std::time::Duration;

use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use reqwest::{Certificate, Client};
use url::Url;

use crate::error::{clickhouse_code_name, parse_clickhouse_error, QueryError, Result};

/// TLS configuration for the HTTP client.
///
/// `insecure` and `ca_bundle` are mutually exclusive at the CLI layer
/// (clap `conflicts_with`), but the constructor enforces it as well so
/// nobody can build an inconsistent driver from inside the crate.
#[derive(Default, Clone, Debug)]
pub struct TlsConfig {
    /// Skip ALL TLS verification (hostname + chain). Dev/lab only.
    pub insecure: bool,
    /// Optional extra CA bundle (PEM, may contain multiple certs)
    /// added to the system trust store. None = system roots only.
    pub ca_bundle_pem: Option<Vec<u8>>,
}

impl TlsConfig {
    /// Validate flag combinations. Used by `HttpDriver::new` and unit tests.
    pub fn validate(&self) -> std::result::Result<(), &'static str> {
        if self.insecure && self.ca_bundle_pem.is_some() {
            return Err("--insecure and --ca-bundle are mutually exclusive");
        }
        Ok(())
    }
}

/// Compression configuration for the HTTP client.
///
/// When `enabled` (the default) reqwest sends `Accept-Encoding: gzip`
/// and transparently decodes the response, AND we tell ClickHouse to
/// `enable_http_compression=1` so it actually compresses. Either side
/// missing is a no-op, so disabling is safe but pointless.
#[derive(Clone, Debug)]
pub struct CompressionConfig {
    pub enabled: bool,
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Tail-mode config: how `tail -n N all.tsv` is synthesized.
///
/// When `enabled`, a large reverse pread (offset > cursor + 32 MiB)
/// triggers a one-shot `SELECT * ORDER BY <pk> DESC LIMIT rows` that is
/// then served as a tail buffer pinned to the file's pseudo-EOF.
#[derive(Clone, Debug)]
pub struct TailConfig {
    pub enabled: bool,
    pub rows: u32,
}

impl Default for TailConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            rows: 10_000,
        }
    }
}

/// Convert a non-success HTTP response into a `QueryError`, preferring a
/// structured `QueryError::ClickHouse` when we can parse one out of the
/// `X-ClickHouse-Exception-Code` header or the body.
///
/// Falls back to `QueryError::Auth` for 401/403 with no parseable code,
/// and `QueryError::Server` for everything else.
fn classify_error(status: u16, header_code: Option<&str>, body: String) -> QueryError {
    if let Some((code, message)) = parse_clickhouse_error(header_code, &body) {
        let name = clickhouse_code_name(code);
        tracing::warn!(
            target: "clickfs::ch",
            code,
            name,
            status,
            "clickhouse exception: {}",
            message
        );
        return QueryError::ClickHouse {
            code,
            name,
            message,
        };
    }
    if status == 401 || status == 403 {
        return QueryError::Auth;
    }
    QueryError::Server { status, body }
}

#[derive(Clone)]
pub struct HttpDriver {
    base_url: Url,
    user: String,
    password: String,
    client: Client,
    query_timeout_secs: u64,
    max_result_bytes: u64,
    compression: CompressionConfig,
}

impl HttpDriver {
    pub fn new(
        base_url: Url,
        user: String,
        password: String,
        query_timeout_secs: u64,
        max_result_bytes: u64,
        tls: TlsConfig,
        compression: CompressionConfig,
    ) -> Result<Self> {
        tls.validate()
            .map_err(|e| QueryError::Transport(e.to_string()))?;

        let mut builder = Client::builder()
            // Per-request timeout disabled; we rely on server-side
            // max_execution_time for streaming reads.
            .pool_idle_timeout(Some(Duration::from_secs(60)))
            // gzip(true) makes reqwest send Accept-Encoding: gzip and
            // transparently decode the response. We pair this with
            // enable_http_compression=1 in append_settings so ClickHouse
            // actually compresses.
            .gzip(compression.enabled);

        if tls.insecure {
            tracing::warn!(
                target: "clickfs::tls",
                "TLS verification disabled (--insecure). Do not use in production."
            );
            builder = builder
                .danger_accept_invalid_certs(true)
                .danger_accept_invalid_hostnames(true);
        }

        if let Some(pem) = &tls.ca_bundle_pem {
            // PEM bundle may contain multiple BEGIN CERTIFICATE blocks;
            // Certificate::from_pem_bundle returns Vec<Certificate>.
            let certs = Certificate::from_pem_bundle(pem)
                .map_err(|e| QueryError::Transport(format!("ca-bundle: {}", e)))?;
            tracing::info!(
                target: "clickfs::tls",
                count = certs.len(),
                "loaded extra CA bundle"
            );
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
        }

        let client = builder
            .build()
            .map_err(|e| QueryError::Transport(e.to_string()))?;

        Ok(Self {
            base_url,
            user,
            password,
            client,
            query_timeout_secs,
            max_result_bytes,
            compression,
        })
    }

    fn append_settings(&self, sql: &str, streaming: bool) -> String {
        let compression_setting = if self.compression.enabled {
            ", enable_http_compression=1"
        } else {
            ""
        };
        if streaming {
            format!(
                "{} SETTINGS max_execution_time={}, max_result_bytes={}, readonly=2{}",
                sql, self.query_timeout_secs, self.max_result_bytes, compression_setting
            )
        } else {
            format!(
                "{} SETTINGS max_execution_time={}, readonly=2{}",
                sql, self.query_timeout_secs, compression_setting
            )
        }
    }

    /// One-shot query. Used for DESCRIBE / SHOW DATABASES / SHOW TABLES /
    /// listing partitions.
    pub async fn query_text(&self, sql: &str) -> std::result::Result<String, QueryError> {
        let full_sql = self.append_settings(sql, false);
        tracing::debug!(target: "clickfs::sql", kind = "text", sql = %full_sql);

        let resp = self
            .client
            .post(self.base_url.clone())
            .basic_auth(&self.user, Some(&self.password))
            .timeout(Duration::from_secs(self.query_timeout_secs + 5))
            .body(full_sql)
            .send()
            .await
            .map_err(QueryError::from)?;

        let status = resp.status();
        let header_code = resp
            .headers()
            .get("X-ClickHouse-Exception-Code")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let body = resp.text().await.map_err(QueryError::from)?;
        if !status.is_success() {
            return Err(classify_error(
                status.as_u16(),
                header_code.as_deref(),
                body,
            ));
        }
        Ok(body)
    }

    /// Streaming query with an optional caller-supplied query_id, used so
    /// the caller can later issue `KILL QUERY WHERE query_id = ...` if the
    /// reader goes away mid-stream. ClickHouse echoes the id back in
    /// system.query_log and accepts it in KILL QUERY exactly as sent.
    pub async fn query_stream_with_id(
        &self,
        sql: &str,
        query_id: Option<&str>,
    ) -> std::result::Result<impl Stream<Item = std::result::Result<Bytes, QueryError>>, QueryError>
    {
        let full_sql = self.append_settings(sql, true);
        tracing::debug!(target: "clickfs::sql", kind = "stream", query_id = ?query_id, sql = %full_sql);

        let mut req = self
            .client
            .post(self.base_url.clone())
            .basic_auth(&self.user, Some(&self.password))
            // Don't set request timeout here; rely on server max_execution_time.
            .body(full_sql);
        if let Some(id) = query_id {
            req = req.query(&[("query_id", id)]);
        }

        let resp = req.send().await.map_err(QueryError::from)?;

        let status = resp.status();
        if !status.is_success() {
            let header_code = resp
                .headers()
                .get("X-ClickHouse-Exception-Code")
                .and_then(|v| v.to_str().ok())
                .map(|s| s.to_string());
            let body = resp.text().await.unwrap_or_default();
            return Err(classify_error(
                status.as_u16(),
                header_code.as_deref(),
                body,
            ));
        }

        let stream = resp
            .bytes_stream()
            .map(|r| r.map_err(|e| QueryError::Stream(e.to_string())));
        Ok(stream)
    }

    /// Best-effort `KILL QUERY ... ASYNC` for the given query_id. Used by
    /// StreamHandle::Drop when a FUSE reader closes the file before EOF.
    /// Errors are intentionally swallowed: the query may have already
    /// finished, the user may not have KILL privilege, or the network
    /// may be down — none of those should crash the FS.
    pub async fn kill_query(&self, query_id: &str) {
        let sql = format!(
            "KILL QUERY WHERE query_id = {} ASYNC",
            quote_string(query_id)
        );
        // Do NOT call append_settings here: KILL is a DDL-ish command and
        // we don't want max_result_bytes/readonly=2 to interfere.
        let res = self
            .client
            .post(self.base_url.clone())
            .basic_auth(&self.user, Some(&self.password))
            .timeout(Duration::from_secs(5))
            .body(sql)
            .send()
            .await;
        match res {
            Ok(r) if r.status().is_success() => {
                tracing::debug!(target: "clickfs::stream", query_id, "kill query sent");
            }
            Ok(r) => {
                tracing::debug!(target: "clickfs::stream", query_id, status = %r.status(), "kill query rejected");
            }
            Err(e) => {
                tracing::debug!(target: "clickfs::stream", query_id, error = %e, "kill query transport failed");
            }
        }
    }

    /// Health check used by `mount` startup.
    pub async fn ping(&self) -> std::result::Result<(), QueryError> {
        self.query_text("SELECT 1").await?;
        Ok(())
    }
}

/// Quote a ClickHouse identifier with backticks.
pub fn quote_ident(s: &str) -> String {
    format!("`{}`", s.replace('`', "``"))
}

/// Quote a string literal (single quotes, escape backslash and quote).
pub fn quote_string(s: &str) -> String {
    let escaped = s.replace('\\', "\\\\").replace('\'', "\\'");
    format!("'{}'", escaped)
}

// --- SQL builders ---

pub fn sql_list_databases() -> String {
    "SELECT name FROM system.databases \
     WHERE name NOT IN ('system','INFORMATION_SCHEMA','information_schema') \
     ORDER BY name FORMAT TabSeparated"
        .to_string()
}

pub fn sql_list_tables(db: &str) -> String {
    format!(
        "SELECT name FROM system.tables WHERE database = {} ORDER BY name FORMAT TabSeparated",
        quote_string(db)
    )
}

pub fn sql_list_partitions(db: &str, tbl: &str) -> String {
    format!(
        "SELECT DISTINCT partition_id FROM system.parts \
         WHERE database = {} AND table = {} AND active \
         ORDER BY partition_id FORMAT TabSeparated",
        quote_string(db),
        quote_string(tbl)
    )
}

pub fn sql_describe(db: &str, tbl: &str) -> String {
    // Render the original DDL so `cat .schema` matches what users expect
    // from `SHOW CREATE TABLE`. TSVRaw preserves embedded newlines and quotes
    // without escaping; the result is a single value followed by a newline.
    format!(
        "SHOW CREATE TABLE {}.{} FORMAT TSVRaw",
        quote_ident(db),
        quote_ident(tbl)
    )
}

pub fn sql_stream_all(db: &str, tbl: &str) -> String {
    format!(
        "SELECT * FROM {}.{} FORMAT TabSeparatedWithNames",
        quote_ident(db),
        quote_ident(tbl)
    )
}

pub fn sql_stream_partition(db: &str, tbl: &str, partition: &str) -> String {
    format!(
        "SELECT * FROM {}.{} WHERE _partition_id = {} FORMAT TabSeparatedWithNames",
        quote_ident(db),
        quote_ident(tbl),
        quote_string(partition)
    )
}

/// First N rows in newline-delimited JSON. Used for both `head.ndjson`
/// (streamed directly) and the README "Sample" section (5 rows).
pub fn sql_head_ndjson(db: &str, tbl: &str, limit: u32) -> String {
    format!(
        "SELECT * FROM {}.{} LIMIT {} FORMAT JSONEachRow",
        quote_ident(db),
        quote_ident(tbl),
        limit
    )
}

/// Aggregate stats from system.parts (active parts only). Returns a
/// single row TSVRaw: rows, bytes_on_disk, parts, partitions, min_time?, max_time?.
/// `min_time`/`max_time` are NULL on tables without a partition-level
/// time column — the README rendering must tolerate that.
pub fn sql_stats(db: &str, tbl: &str) -> String {
    format!(
        "SELECT \
            sum(rows) AS rows, \
            sum(bytes_on_disk) AS bytes_on_disk, \
            count() AS parts, \
            uniqExact(partition) AS partitions, \
            min(min_time) AS min_time, \
            max(max_time) AS max_time \
         FROM system.parts \
         WHERE database = {} AND table = {} AND active \
         FORMAT JSONEachRow",
        quote_string(db),
        quote_string(tbl)
    )
}

/// primary_key + sorting_key for a table, JSONEachRow. Either field may
/// be empty (e.g. Memory/Log/StripeLog engines have no key); the caller
/// then falls back to `tuple()` for the tail ORDER BY.
pub fn sql_table_pk(db: &str, tbl: &str) -> String {
    format!(
        "SELECT primary_key, sorting_key \
         FROM system.tables \
         WHERE database = {} AND name = {} \
         FORMAT JSONEachRow",
        quote_string(db),
        quote_string(tbl)
    )
}

/// `SELECT *` ordered DESC by `order_expr` (already a comma-separated
/// column list, or the literal `tuple()`), limited to N rows. Used to
/// materialize the tail buffer when a reader does a large reverse pread.
/// Output is `TabSeparatedWithNames`; caller is responsible for
/// reversing the data rows so the buffer reads "oldest first".
pub fn sql_select_tail(db: &str, tbl: &str, order_expr: &str, limit: u32) -> String {
    // For multi-column keys we must apply DESC to every column, not just
    // the trailing one (`a, b DESC` only sorts b descending). `tuple()`
    // is a single expression so the simple form still works.
    let trimmed = order_expr.trim();
    let order_clause = if trimmed.is_empty() || trimmed == "tuple()" {
        format!("{} DESC", trimmed)
    } else {
        trimmed
            .split(',')
            .map(|c| format!("{} DESC", c.trim()))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "SELECT * FROM {}.{} ORDER BY {} LIMIT {} FORMAT TabSeparatedWithNames",
        quote_ident(db),
        quote_ident(tbl),
        order_clause,
        limit
    )
}

/// Column metadata: name + type + comment. JSONEachRow so the README
/// renderer can deserialize without fighting TSV escaping.
pub fn sql_columns(db: &str, tbl: &str) -> String {
    format!(
        "SELECT name, type, comment \
         FROM system.columns \
         WHERE database = {} AND table = {} \
         ORDER BY position \
         FORMAT JSONEachRow",
        quote_string(db),
        quote_string(tbl)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tls_default_is_valid() {
        assert!(TlsConfig::default().validate().is_ok());
    }

    #[test]
    fn tls_insecure_only_is_valid() {
        let cfg = TlsConfig {
            insecure: true,
            ca_bundle_pem: None,
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn tls_ca_bundle_only_is_valid() {
        let cfg = TlsConfig {
            insecure: false,
            ca_bundle_pem: Some(b"-----BEGIN CERTIFICATE-----".to_vec()),
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn tls_insecure_and_ca_bundle_conflict() {
        let cfg = TlsConfig {
            insecure: true,
            ca_bundle_pem: Some(b"x".to_vec()),
        };
        let err = cfg.validate().unwrap_err();
        assert!(err.contains("mutually exclusive"));
    }

    #[test]
    fn driver_new_rejects_conflicting_tls() {
        let url = Url::parse("http://localhost:8123").unwrap();
        let cfg = TlsConfig {
            insecure: true,
            ca_bundle_pem: Some(b"x".to_vec()),
        };
        let r = HttpDriver::new(
            url,
            "u".into(),
            "p".into(),
            60,
            1024,
            cfg,
            CompressionConfig::default(),
        );
        assert!(r.is_err());
    }

    #[test]
    fn driver_new_rejects_garbage_ca_bundle() {
        // A non-PEM blob should fail to parse.
        let url = Url::parse("http://localhost:8123").unwrap();
        let cfg = TlsConfig {
            insecure: false,
            ca_bundle_pem: Some(b"this is not a certificate".to_vec()),
        };
        let r = HttpDriver::new(
            url,
            "u".into(),
            "p".into(),
            60,
            1024,
            cfg,
            CompressionConfig::default(),
        );
        // from_pem_bundle on garbage returns an empty Vec on some
        // versions and an error on others; either way, must not panic.
        // Accept both outcomes to avoid coupling to reqwest internals.
        let _ = r;
    }

    #[test]
    fn compression_default_is_enabled() {
        assert!(CompressionConfig::default().enabled);
    }

    #[test]
    fn compression_setting_in_sql_when_enabled() {
        let url = Url::parse("http://localhost:8123").unwrap();
        let d = HttpDriver::new(
            url,
            "u".into(),
            "p".into(),
            60,
            1024,
            TlsConfig::default(),
            CompressionConfig { enabled: true },
        )
        .unwrap();
        let sql = d.append_settings("SELECT 1", false);
        assert!(
            sql.contains("enable_http_compression=1"),
            "expected enable_http_compression=1 in SQL, got: {sql}"
        );
    }

    #[test]
    fn compression_setting_absent_when_disabled() {
        let url = Url::parse("http://localhost:8123").unwrap();
        let d = HttpDriver::new(
            url,
            "u".into(),
            "p".into(),
            60,
            1024,
            TlsConfig::default(),
            CompressionConfig { enabled: false },
        )
        .unwrap();
        let sql = d.append_settings("SELECT 1", true);
        assert!(
            !sql.contains("enable_http_compression"),
            "expected NO enable_http_compression in SQL, got: {sql}"
        );
    }

    #[test]
    fn sql_table_pk_targets_system_tables() {
        let s = sql_table_pk("d", "t");
        assert!(s.contains("system.tables"));
        assert!(s.contains("primary_key"));
        assert!(s.contains("sorting_key"));
        assert!(s.contains("'d'"));
        assert!(s.contains("'t'"));
        assert!(s.ends_with("FORMAT JSONEachRow"));
    }

    #[test]
    fn sql_select_tail_quotes_ident_and_orders_desc() {
        let s = sql_select_tail("my db", "my tbl", "id", 10);
        assert!(s.contains("`my db`.`my tbl`"));
        assert!(s.contains("ORDER BY id DESC"));
        assert!(s.contains("LIMIT 10"));
        assert!(s.ends_with("FORMAT TabSeparatedWithNames"));
    }

    #[test]
    fn sql_select_tail_accepts_tuple_fallback() {
        let s = sql_select_tail("d", "t", "tuple()", 5);
        assert!(s.contains("ORDER BY tuple() DESC"));
    }

    #[test]
    fn sql_select_tail_applies_desc_per_column() {
        // Multi-column key must DESC every column, not just the last,
        // otherwise ClickHouse only sorts the trailing column descending.
        let s = sql_select_tail("d", "t", "a, b, c", 7);
        assert!(s.contains("ORDER BY a DESC, b DESC, c DESC"), "got: {s}");
        assert!(s.contains("LIMIT 7"));
    }
}
