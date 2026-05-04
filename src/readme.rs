//! Markdown renderer for the per-table `README.md` pseudo-file.
//!
//! The file is synthesized on each `open()` (no caching) by issuing 5
//! sub-queries concurrently and merging the results into a single
//! markdown document. Any sub-query that fails turns into an
//! "_(unavailable)_" line — the README is best-effort by design so
//! that a missing privilege on `system.parts` doesn't black-hole the
//! whole file.
//!
//! Layout (locked by P2 plan):
//!
//!   # `<db>`.`<tbl>`
//!   ## Stats
//!   ## Schema
//!   ## Columns
//!   ## Sample (5 rows)
//!   ## Example queries
//!   ## Files

use std::fmt::Write;

use serde_json::Value;

use crate::driver::{self, HttpDriver};

/// Concurrently fetch the 5 inputs we need and render markdown.
/// Always returns a String (never errors): individual section failures
/// are rendered inline.
pub async fn render(driver: &HttpDriver, db: &str, tbl: &str) -> String {
    // Build all SQL strings up front so we don't borrow temporaries
    // across the tokio::join! await.
    let sql_describe = driver::sql_describe(db, tbl);
    let sql_stats = driver::sql_stats(db, tbl);
    let sql_columns = driver::sql_columns(db, tbl);
    let sql_count = format!(
        "SELECT count() FROM {}.{} FORMAT TabSeparated",
        driver::quote_ident(db),
        driver::quote_ident(tbl)
    );
    let sql_sample = driver::sql_head_ndjson(db, tbl, 5);

    // Fire all five queries in parallel. Per-query timeouts are bounded
    // by the driver's max_execution_time setting.
    let (schema, stats, columns, count, sample) = tokio::join!(
        driver.query_text(&sql_describe),
        driver.query_text(&sql_stats),
        driver.query_text(&sql_columns),
        driver.query_text(&sql_count),
        driver.query_text(&sql_sample),
    );

    let mut out = String::with_capacity(2048);
    let _ = writeln!(out, "# `{}`.`{}`", db, tbl);
    out.push('\n');

    // ---- Stats ----
    out.push_str("## Stats\n\n");
    match (stats.as_ref(), count.as_ref()) {
        (Ok(s), Ok(c)) => render_stats(&mut out, s, c.trim()),
        (_, Ok(c)) => {
            // Stats from system.parts unavailable but we still have row count.
            let _ = writeln!(out, "- **rows**: {}", c.trim());
            out.push_str("- **size on disk**: _(unavailable)_\n");
            out.push_str("- **parts**: _(unavailable)_\n");
            out.push_str("- **partitions**: _(unavailable)_\n");
        }
        _ => out.push_str("_(unavailable)_\n"),
    }
    out.push('\n');

    // ---- Schema ----
    out.push_str("## Schema\n\n");
    match schema.as_ref() {
        Ok(s) => {
            out.push_str("```sql\n");
            out.push_str(s.trim_end());
            out.push_str("\n```\n");
        }
        Err(_) => out.push_str("_(unavailable)_\n"),
    }
    out.push('\n');

    // ---- Columns ----
    out.push_str("## Columns\n\n");
    match columns.as_ref() {
        Ok(c) => render_columns(&mut out, c),
        Err(_) => out.push_str("_(unavailable)_\n"),
    }
    out.push('\n');

    // ---- Sample ----
    out.push_str("## Sample (5 rows)\n\n");
    match sample.as_ref() {
        Ok(s) if !s.trim().is_empty() => {
            out.push_str("```jsonl\n");
            out.push_str(s.trim_end());
            out.push_str("\n```\n");
        }
        Ok(_) => out.push_str("_(empty)_\n"),
        Err(_) => out.push_str("_(unavailable)_\n"),
    }
    out.push('\n');

    // ---- Example queries ----
    out.push_str("## Example queries\n\n");
    let _ = writeln!(
        out,
        "Sequential scan over the whole table:\n\
         ```bash\n\
         cat /<mountpoint>/db/{db}/{tbl}/all.tsv | head -n 100\n\
         ```\n\n\
         Inspect a single partition:\n\
         ```bash\n\
         cat /<mountpoint>/db/{db}/{tbl}/<partition_id>.tsv\n\
         ```\n\n\
         First 100 rows as JSON, easy to pipe to `jq`:\n\
         ```bash\n\
         cat /<mountpoint>/db/{db}/{tbl}/head.ndjson | jq .\n\
         ```\n",
        db = db,
        tbl = tbl
    );
    out.push('\n');

    // ---- Files ----
    out.push_str("## Files\n\n");
    out.push_str("| File | Description |\n");
    out.push_str("|---|---|\n");
    out.push_str("| `.schema` | `SHOW CREATE TABLE` output |\n");
    out.push_str("| `README.md` | This file (regenerated on open) |\n");
    out.push_str("| `head.ndjson` | First N rows as JSON (one per line) |\n");
    out.push_str("| `all.tsv` | Full table stream, TSV with header |\n");
    out.push_str("| `<partition>.tsv` | Single partition stream |\n");

    out
}

/// Render the Stats section. `stats_json` is a single JSONEachRow line
/// from sql_stats; `count_str` is the trimmed COUNT() value.
fn render_stats(out: &mut String, stats_json: &str, count_str: &str) {
    // Try to parse the JSONEachRow line. If empty (no parts) or
    // malformed, fall back to count-only.
    let trimmed = stats_json.trim();
    if trimmed.is_empty() {
        let _ = writeln!(out, "- **rows**: {}", count_str);
        out.push_str("- **size on disk**: 0 (no active parts)\n");
        return;
    }

    let v: Value = match serde_json::from_str(trimmed) {
        Ok(v) => v,
        Err(_) => {
            let _ = writeln!(out, "- **rows**: {}", count_str);
            return;
        }
    };

    let rows = v.get("rows").and_then(json_to_u64);
    let bytes = v.get("bytes_on_disk").and_then(json_to_u64);
    let parts = v.get("parts").and_then(json_to_u64);
    let parts_count = v.get("partitions").and_then(json_to_u64);

    // Prefer the live count() over sum(rows) from system.parts: the
    // latter excludes inserts that haven't merged yet.
    let _ = writeln!(out, "- **rows**: {}", count_str);
    if let Some(b) = bytes {
        let _ = writeln!(out, "- **size on disk**: {} ({})", human_bytes(b), b);
    }
    if let Some(p) = parts {
        let _ = writeln!(out, "- **active parts**: {}", p);
    }
    if let Some(p) = parts_count {
        let _ = writeln!(out, "- **partitions**: {}", p);
    }
    if let (Some(rows), Some(bytes)) = (rows, bytes) {
        if rows > 0 {
            let bpr = bytes as f64 / rows as f64;
            let _ = writeln!(out, "- **avg bytes/row**: {:.1}", bpr);
        }
    }
}

fn render_columns(out: &mut String, jsonl: &str) {
    out.push_str("| Name | Type | Comment |\n");
    out.push_str("|---|---|---|\n");
    let mut any = false;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let name = v.get("name").and_then(|x| x.as_str()).unwrap_or("");
        let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
        let comment = v.get("comment").and_then(|x| x.as_str()).unwrap_or("");
        // Escape pipes so they don't break the table.
        let name = name.replace('|', "\\|");
        let ty = ty.replace('|', "\\|");
        let comment = comment.replace('|', "\\|").replace('\n', " ");
        let _ = writeln!(out, "| `{}` | `{}` | {} |", name, ty, comment);
        any = true;
    }
    if !any {
        out.push_str("_(no columns)_\n");
    }
}

/// JSONEachRow numbers can come back as integers or as quoted strings
/// (UInt64 etc are stringified by ClickHouse to avoid f64 precision
/// loss). Accept both.
fn json_to_u64(v: &Value) -> Option<u64> {
    if let Some(n) = v.as_u64() {
        return Some(n);
    }
    if let Some(s) = v.as_str() {
        return s.parse().ok();
    }
    None
}

/// Pretty-print a byte count. We avoid pulling in a crate for this.
fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{} {}", n, UNITS[0])
    } else {
        format!("{:.2} {}", v, UNITS[i])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.00 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(human_bytes(1024u64.pow(4)), "1.00 TiB");
    }

    #[test]
    fn json_to_u64_accepts_int_and_string() {
        assert_eq!(json_to_u64(&Value::from(42u64)), Some(42));
        assert_eq!(json_to_u64(&Value::from("9999")), Some(9999));
        assert_eq!(json_to_u64(&Value::Null), None);
        assert_eq!(json_to_u64(&Value::from("not a number")), None);
    }

    #[test]
    fn render_columns_handles_empty_and_pipes() {
        let mut out = String::new();
        render_columns(&mut out, "");
        assert!(out.contains("_(no columns)_"));

        out.clear();
        render_columns(
            &mut out,
            r#"{"name":"a","type":"Int32","comment":""}
{"name":"b|c","type":"String","comment":"has | pipe"}"#,
        );
        assert!(out.contains("| `a` | `Int32` |"));
        assert!(out.contains(r"b\|c"));
        assert!(out.contains(r"has \| pipe"));
    }

    #[test]
    fn render_stats_with_no_parts_uses_count_only() {
        let mut out = String::new();
        render_stats(&mut out, "", "0");
        assert!(out.contains("**rows**: 0"));
        assert!(out.contains("no active parts"));
    }

    #[test]
    fn render_stats_with_real_parts_emits_size_and_avg() {
        let mut out = String::new();
        render_stats(
            &mut out,
            r#"{"rows":"1000","bytes_on_disk":"4096","parts":"1","partitions":"1"}"#,
            "1000",
        );
        assert!(out.contains("**rows**: 1000"));
        assert!(out.contains("**size on disk**: 4.00 KiB"));
        assert!(out.contains("**active parts**: 1"));
        assert!(out.contains("**avg bytes/row**: 4.1"));
    }
}
