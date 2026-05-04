//! ClickHouse HTTP driver.
//!
//! - `query_text`  : small queries returning a full text body.
//! - `query_stream`: streaming queries returning a Bytes stream.

use std::time::Duration;

use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use reqwest::Client;
use url::Url;

use crate::error::{QueryError, Result};

#[derive(Clone)]
pub struct HttpDriver {
    base_url: Url,
    user: String,
    password: String,
    client: Client,
    query_timeout_secs: u64,
    max_result_bytes: u64,
}

impl HttpDriver {
    pub fn new(
        base_url: Url,
        user: String,
        password: String,
        query_timeout_secs: u64,
        max_result_bytes: u64,
    ) -> Result<Self> {
        let client = Client::builder()
            // Per-request timeout disabled; we rely on server-side
            // max_execution_time for streaming reads.
            .pool_idle_timeout(Some(Duration::from_secs(60)))
            .build()
            .map_err(|e| QueryError::Transport(e.to_string()))?;

        Ok(Self {
            base_url,
            user,
            password,
            client,
            query_timeout_secs,
            max_result_bytes,
        })
    }

    fn append_settings(&self, sql: &str, streaming: bool) -> String {
        if streaming {
            format!(
                "{} SETTINGS max_execution_time={}, max_result_bytes={}, readonly=2",
                sql, self.query_timeout_secs, self.max_result_bytes
            )
        } else {
            format!(
                "{} SETTINGS max_execution_time={}, readonly=2",
                sql, self.query_timeout_secs
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
        let body = resp.text().await.map_err(QueryError::from)?;
        if !status.is_success() {
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(QueryError::Auth);
            }
            return Err(QueryError::Server {
                status: status.as_u16(),
                body,
            });
        }
        Ok(body)
    }

    /// Streaming query. Returns a stream of Bytes chunks; dropping the stream
    /// closes the underlying HTTP connection (best-effort cancel).
    pub async fn query_stream(
        &self,
        sql: &str,
    ) -> std::result::Result<impl Stream<Item = std::result::Result<Bytes, QueryError>>, QueryError>
    {
        let full_sql = self.append_settings(sql, true);
        tracing::debug!(target: "clickfs::sql", kind = "stream", sql = %full_sql);

        let resp = self
            .client
            .post(self.base_url.clone())
            .basic_auth(&self.user, Some(&self.password))
            // Don't set request timeout here; rely on server max_execution_time.
            .body(full_sql)
            .send()
            .await
            .map_err(QueryError::from)?;

        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            if status.as_u16() == 401 || status.as_u16() == 403 {
                return Err(QueryError::Auth);
            }
            return Err(QueryError::Server {
                status: status.as_u16(),
                body,
            });
        }

        let stream = resp
            .bytes_stream()
            .map(|r| r.map_err(|e| QueryError::Stream(e.to_string())));
        Ok(stream)
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

pub fn sql_exists_database(db: &str) -> String {
    format!(
        "SELECT count() FROM system.databases WHERE name = {} FORMAT TabSeparated",
        quote_string(db)
    )
}

pub fn sql_exists_table(db: &str, tbl: &str) -> String {
    format!(
        "SELECT count() FROM system.tables WHERE database = {} AND name = {} FORMAT TabSeparated",
        quote_string(db),
        quote_string(tbl)
    )
}

pub fn sql_exists_partition(db: &str, tbl: &str, partition: &str) -> String {
    format!(
        "SELECT count() FROM system.parts WHERE database = {} AND table = {} AND active AND partition_id = {} FORMAT TabSeparated",
        quote_string(db),
        quote_string(tbl),
        quote_string(partition)
    )
}
