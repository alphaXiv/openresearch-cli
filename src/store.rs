//! Local run store — orx's own truth for externally-executed runs.
//!
//! Mirrors the opencode model: state lives in a SQLite db beside the work
//! (`orx.db` under the data dir), `orx serve` exposes it over loopback
//! HTTP/SSE, and the api snapshots the whole dir to R2 per project. Run logs
//! are plain append-only files under `run-logs/<runId>.log` so tailing (serve)
//! and appending (supervise) never contend on the db.
//!
//! Data dir: `$ORX_DATA_DIR`, else `$XDG_DATA_HOME/openresearch`, else
//! `~/.local/share/openresearch` — the exact path the api's snapshot/restore
//! tars on agent boxes.

use std::path::PathBuf;

use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

use crate::error::{anyhow, Result};

pub fn data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("ORX_DATA_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".local")
                .join("share")
        });
    base.join("openresearch")
}

pub fn log_path(run_id: &str) -> PathBuf {
    // Run ids are server-issued UUIDs; sanitize anyway so a hostile id can't
    // escape the log dir.
    let safe: String = run_id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    data_dir().join("run-logs").join(format!("{safe}.log"))
}

/// A locally-tracked external run. `status` uses the server vocabulary
/// (starting/running/done/failed/cancelled); `backend_json` is the opaque
/// descriptor (kind, namespace, jobId, flavor…) shared with the api mirror.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StoredRun {
    pub id: String,
    pub experiment_id: String,
    pub project_id: String,
    pub status: String,
    pub backend_json: String,
    pub command: String,
    /// Unix millis.
    pub created_at: i64,
    pub updated_at: i64,
    pub ended_at: Option<i64>,
    pub exit_code: Option<i64>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating dirs/schema as needed). WAL so the supervise writers and
    /// the serve readers never block each other.
    pub fn open() -> Result<Self> {
        let dir = data_dir();
        std::fs::create_dir_all(dir.join("run-logs"))
            .map_err(|e| anyhow!("Could not create {}: {}", dir.display(), e))?;
        let conn = Connection::open(dir.join("orx.db"))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                id           TEXT PRIMARY KEY,
                experiment_id TEXT NOT NULL,
                project_id   TEXT NOT NULL,
                status       TEXT NOT NULL,
                backend_json TEXT NOT NULL,
                command      TEXT NOT NULL DEFAULT '',
                created_at   INTEGER NOT NULL,
                updated_at   INTEGER NOT NULL,
                ended_at     INTEGER,
                exit_code    INTEGER
            );",
        )?;
        Ok(Self { conn })
    }

    pub fn upsert_run(&self, run: &StoredRun) -> Result<()> {
        self.conn.execute(
            "INSERT INTO runs (id, experiment_id, project_id, status, backend_json, command,
                               created_at, updated_at, ended_at, exit_code)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
             ON CONFLICT(id) DO UPDATE SET
               status = excluded.status,
               backend_json = excluded.backend_json,
               updated_at = excluded.updated_at,
               ended_at = excluded.ended_at,
               exit_code = excluded.exit_code",
            params![
                run.id,
                run.experiment_id,
                run.project_id,
                run.status,
                run.backend_json,
                run.command,
                run.created_at,
                run.updated_at,
                run.ended_at,
                run.exit_code,
            ],
        )?;
        Ok(())
    }

    pub fn update_status(
        &self,
        run_id: &str,
        status: &str,
        ended_at: Option<i64>,
        exit_code: Option<i64>,
    ) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET status = ?2, updated_at = ?3, ended_at = COALESCE(?4, ended_at),
                             exit_code = COALESCE(?5, exit_code)
             WHERE id = ?1",
            params![run_id, status, now_ms(), ended_at, exit_code],
        )?;
        Ok(())
    }

    pub fn get_run(&self, run_id: &str) -> Result<Option<StoredRun>> {
        let run = self
            .conn
            .query_row(
                &format!("{SELECT_RUN} WHERE id = ?1"),
                params![run_id],
                row_to_run,
            )
            .optional()?;
        Ok(run)
    }

    /// Newest first (creation time).
    pub fn list_runs(&self, limit: usize) -> Result<Vec<StoredRun>> {
        let mut stmt = self
            .conn
            .prepare(&format!("{SELECT_RUN} ORDER BY created_at DESC LIMIT ?1"))?;
        let rows = stmt.query_map(params![limit as i64], row_to_run)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

const SELECT_RUN: &str = "SELECT id, experiment_id, project_id, status, backend_json, command,
                                 created_at, updated_at, ended_at, exit_code FROM runs";

fn row_to_run(row: &rusqlite::Row<'_>) -> std::result::Result<StoredRun, rusqlite::Error> {
    Ok(StoredRun {
        id: row.get(0)?,
        experiment_id: row.get(1)?,
        project_id: row.get(2)?,
        status: row.get(3)?,
        backend_json: row.get(4)?,
        command: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        ended_at: row.get(8)?,
        exit_code: row.get(9)?,
    })
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}
