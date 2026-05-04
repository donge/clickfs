//! Path → QueryPlan resolver.
//!
//! Strictly syntactic: identifier validation only, no ClickHouse access.

use std::path::Path;

use crate::error::{ClickFsError, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanKind {
    Root,           // "/"
    DbNamespace,    // "/db"
    ListTables,     // "/db/<db>"
    ListPartitions, // "/db/<db>/<tbl>"  (dir; readdir merges parts + dotfiles + all.tsv)
    DescribeTable,  // "/db/<db>/<tbl>/.schema"
    /// "/db/<db>/<tbl>/README.md" — AI-friendly markdown summary
    /// (schema + stats + sample + example queries). Synthesized by
    /// concurrently issuing 5 sub-queries; rendered in-memory.
    Readme,
    /// "/db/<db>/<tbl>/head.ndjson" — first N rows in JSONEachRow.
    /// Streamed (no full materialization) but bounded by LIMIT.
    HeadNdjson,
    StreamAll,               // "/db/<db>/<tbl>/all.tsv"
    StreamPartition(String), // "/db/<db>/<tbl>/<partition>.tsv"
}

#[derive(Debug, Clone)]
pub struct QueryPlan {
    pub kind: PlanKind,
    pub db: Option<String>,
    pub table: Option<String>,
}

impl QueryPlan {
    pub fn is_dir(&self) -> bool {
        matches!(
            self.kind,
            PlanKind::Root
                | PlanKind::DbNamespace
                | PlanKind::ListTables
                | PlanKind::ListPartitions
        )
    }

    pub fn is_special_file(&self) -> bool {
        matches!(self.kind, PlanKind::DescribeTable | PlanKind::Readme)
    }

    #[allow(dead_code)]
    pub fn is_stream_file(&self) -> bool {
        matches!(
            self.kind,
            PlanKind::StreamAll | PlanKind::StreamPartition(_) | PlanKind::HeadNdjson
        )
    }
}

/// Validate a ClickHouse identifier (database, table, partition_id-as-filename).
fn valid_identifier(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 192
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
    // identifiers proper (db/table) must not start with '.', but partition_ids may.
    // We allow '.' here and let the caller filter dotfiles separately.
}

fn valid_db_or_table(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 192
        && !s.starts_with('.')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn resolve(path: &Path) -> Result<QueryPlan> {
    let comps: Vec<&str> = path
        .components()
        .filter_map(|c| match c {
            std::path::Component::RootDir => None,
            std::path::Component::Normal(s) => s.to_str(),
            _ => None, // CurDir / ParentDir / Prefix → reject
        })
        .collect();

    // Reject if any component contained "..":
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(ClickFsError::NotFound);
    }

    match comps.as_slice() {
        [] => Ok(QueryPlan {
            kind: PlanKind::Root,
            db: None,
            table: None,
        }),
        ["db"] => Ok(QueryPlan {
            kind: PlanKind::DbNamespace,
            db: None,
            table: None,
        }),
        ["db", db] => {
            if !valid_db_or_table(db) {
                return Err(ClickFsError::InvalidIdentifier(db.to_string()));
            }
            Ok(QueryPlan {
                kind: PlanKind::ListTables,
                db: Some(db.to_string()),
                table: None,
            })
        }
        ["db", db, tbl] => {
            if !valid_db_or_table(db) || !valid_db_or_table(tbl) {
                return Err(ClickFsError::InvalidIdentifier(format!("{}.{}", db, tbl)));
            }
            Ok(QueryPlan {
                kind: PlanKind::ListPartitions,
                db: Some(db.to_string()),
                table: Some(tbl.to_string()),
            })
        }
        ["db", db, tbl, leaf] => {
            if !valid_db_or_table(db) || !valid_db_or_table(tbl) {
                return Err(ClickFsError::InvalidIdentifier(format!("{}.{}", db, tbl)));
            }
            let db = db.to_string();
            let tbl = tbl.to_string();
            match *leaf {
                ".schema" => Ok(QueryPlan {
                    kind: PlanKind::DescribeTable,
                    db: Some(db),
                    table: Some(tbl),
                }),
                "README.md" => Ok(QueryPlan {
                    kind: PlanKind::Readme,
                    db: Some(db),
                    table: Some(tbl),
                }),
                "head.ndjson" => Ok(QueryPlan {
                    kind: PlanKind::HeadNdjson,
                    db: Some(db),
                    table: Some(tbl),
                }),
                "all.tsv" => Ok(QueryPlan {
                    kind: PlanKind::StreamAll,
                    db: Some(db),
                    table: Some(tbl),
                }),
                name if name.ends_with(".tsv") => {
                    let part = &name[..name.len() - 4];
                    if !valid_identifier(part) {
                        return Err(ClickFsError::InvalidIdentifier(part.to_string()));
                    }
                    Ok(QueryPlan {
                        kind: PlanKind::StreamPartition(part.to_string()),
                        db: Some(db),
                        table: Some(tbl),
                    })
                }
                _ => Err(ClickFsError::NotFound),
            }
        }
        _ => Err(ClickFsError::NotFound),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn r(s: &str) -> Result<QueryPlan> {
        resolve(&PathBuf::from(s))
    }

    #[test]
    fn root() {
        assert!(matches!(r("/").unwrap().kind, PlanKind::Root));
    }
    #[test]
    fn db_ns() {
        assert!(matches!(r("/db").unwrap().kind, PlanKind::DbNamespace));
    }
    #[test]
    fn list_tables() {
        let p = r("/db/default").unwrap();
        assert!(matches!(p.kind, PlanKind::ListTables));
        assert_eq!(p.db.as_deref(), Some("default"));
    }
    #[test]
    fn schema_file() {
        let p = r("/db/default/users/.schema").unwrap();
        assert!(matches!(p.kind, PlanKind::DescribeTable));
    }
    #[test]
    fn all_tsv() {
        let p = r("/db/default/users/all.tsv").unwrap();
        assert!(matches!(p.kind, PlanKind::StreamAll));
    }
    #[test]
    fn partition_tsv() {
        let p = r("/db/default/users/202601.tsv").unwrap();
        match p.kind {
            PlanKind::StreamPartition(s) => assert_eq!(s, "202601"),
            _ => panic!(),
        }
    }
    #[test]
    fn reject_parent() {
        assert!(r("/db/../etc").is_err());
    }
    #[test]
    fn reject_bad_id() {
        assert!(r("/db/foo;bar").is_err());
    }

    #[test]
    fn readme_md() {
        let p = r("/db/default/users/README.md").unwrap();
        assert!(matches!(p.kind, PlanKind::Readme));
        assert_eq!(p.db.as_deref(), Some("default"));
        assert_eq!(p.table.as_deref(), Some("users"));
    }

    #[test]
    fn head_ndjson() {
        let p = r("/db/default/users/head.ndjson").unwrap();
        assert!(matches!(p.kind, PlanKind::HeadNdjson));
    }
}
