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
use crate::local::model::{LocalExperiment, LocalProject};

pub fn data_dir() -> PathBuf {
    // Resolution order (most to least authoritative):
    //   1. $ORX_DATA_DIR — explicit imperative override (launch.json, tests,
    //      the Codex sandbox pin). Stays on top so a forced path always wins.
    //   2. persisted user choice (config_dir()/settings.json `dataDir`) — set
    //      from the UI's Storage settings. Read fresh every call (no cache) so a
    //      just-completed data-dir move is picked up by the next Store::open().
    //   3. $XDG_DATA_HOME/openresearch — ambient system default *base*; an
    //      explicit UI choice rightly beats it, so it sits below (2).
    //   4. ~/.local/share/openresearch — hardcoded default.
    if let Some(dir) = env_path("ORX_DATA_DIR") {
        return dir;
    }
    if let Some(dir) = crate::config::settings_data_dir() {
        return dir;
    }
    xdg_default_data_dir()
}

/// Read an env var as a path, treating unset **and empty** the same (an empty
/// `export ORX_DATA_DIR=` is a shell footgun that must not resolve to `""`).
fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `$XDG_DATA_HOME/openresearch` else `~/.local/share/openresearch` — the tail
/// of the resolution chain, shared by `data_dir()` and `default_data_dir()`.
fn xdg_default_data_dir() -> PathBuf {
    let base = env_path("XDG_DATA_HOME").unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".local")
            .join("share")
    });
    base.join("openresearch")
}

/// The data dir ignoring any persisted user choice — where resolution would
/// land if `settings.json` had no `dataDir`. Used by the Storage UI to show the
/// "(default)" path and offer resetting to it. `$ORX_DATA_DIR` still wins, since
/// it's a forced override.
pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = env_path("ORX_DATA_DIR") {
        return dir;
    }
    xdg_default_data_dir()
}

/// Where `data_dir()`'s answer came from — surfaced by the Storage settings API
/// so the UI can explain a forced env override (read-only) vs. a user choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DataDirSource {
    /// `$ORX_DATA_DIR` is set — forces the path, UI field is read-only.
    Env,
    /// Persisted user choice in `settings.json`.
    Config,
    /// Derived from `$XDG_DATA_HOME` (no user choice).
    Xdg,
    /// Hardcoded `~/.local/share/openresearch`.
    Default,
}

/// Classify the current `data_dir()` resolution for the Storage settings UI.
pub fn data_dir_source() -> DataDirSource {
    if env_path("ORX_DATA_DIR").is_some() {
        return DataDirSource::Env;
    }
    if crate::config::settings_data_dir().is_some() {
        return DataDirSource::Config;
    }
    if env_path("XDG_DATA_HOME").is_some() {
        return DataDirSource::Xdg;
    }
    DataDirSource::Default
}

/// Compact human-readable byte size (e.g. `1.2 KB`, `3.4 MB`). Shared by the
/// artifacts listing and the data-dir move so the two don't drift.
pub fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if n < 1024 {
        return format!("{n} B");
    }
    let mut size = n as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    format!("{:.1} {}", size, UNITS[unit])
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
    pub commit_sha: Option<String>,
    pub result_markdown: Option<String>,
    /// Local-mode cancel intent (the supervisor polls it; server runs ignore it).
    pub cancel_requested: bool,
    /// The `orx up` chat session that launched this run, when it was started by
    /// an agent harness child (which exports `ORX_CHAT_SESSION_ID`). `None` for
    /// CLI-launched or server runs. The run watcher routes the completion
    /// notification to exactly this session — never a project-wide guess.
    pub chat_session_id: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating dirs/schema as needed). WAL so the supervise writers and
    /// the serve readers never block each other.
    pub fn open() -> Result<Self> {
        Self::open_at(data_dir())
    }

    /// Open a store rooted at an explicit directory, bypassing `data_dir()`
    /// resolution. For tests: a throwaway temp dir here avoids mutating the
    /// process-global `$ORX_DATA_DIR`, which the localbox lifecycle test owns
    /// (tests in different modules share env under the parallel runner).
    pub fn open_at(dir: PathBuf) -> Result<Self> {
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
            );
            CREATE TABLE IF NOT EXISTS local_projects (
                id              TEXT PRIMARY KEY,
                name            TEXT NOT NULL,
                slug            TEXT NOT NULL UNIQUE,
                github_owner    TEXT NOT NULL,
                github_repo     TEXT NOT NULL,
                baseline_branch TEXT NOT NULL DEFAULT 'main',
                repo_path       TEXT NOT NULL,
                run_command     TEXT,
                paper_id        TEXT,
                created_at      INTEGER NOT NULL,
                updated_at      INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS local_experiments (
                id                   TEXT PRIMARY KEY,
                project_id           TEXT NOT NULL,
                parent_experiment_id TEXT,
                slug                 TEXT NOT NULL,
                branch_name          TEXT NOT NULL,
                title                TEXT,
                description          TEXT,
                run_command          TEXT NOT NULL,
                agent_status         TEXT NOT NULL DEFAULT 'idle',
                created_at           INTEGER NOT NULL,
                updated_at           INTEGER NOT NULL,
                UNIQUE(project_id, slug)
            );
            DROP TABLE IF EXISTS local_reports;
            CREATE TABLE IF NOT EXISTS chat_sessions (
                id                TEXT PRIMARY KEY,
                project_id        TEXT NOT NULL,
                harness           TEXT NOT NULL,
                native_session_id TEXT,
                title             TEXT,
                model             TEXT,
                permission_mode   TEXT,
                reasoning_level   TEXT,
                archived          INTEGER NOT NULL DEFAULT 0,
                context_usage_json TEXT,
                created_at        INTEGER NOT NULL,
                updated_at        INTEGER NOT NULL
            );
            CREATE TABLE IF NOT EXISTS chat_messages (
                id         TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                role       TEXT NOT NULL,
                parts_json TEXT NOT NULL,
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_chat_messages_session
                ON chat_messages(session_id, created_at);
            CREATE TABLE IF NOT EXISTS ssh_host_tests (
                host      TEXT PRIMARY KEY,
                reachable INTEGER NOT NULL,
                git_found INTEGER NOT NULL,
                error     TEXT,
                tested_at INTEGER NOT NULL
            );",
        )?;
        // Best-effort migrations for pre-existing dbs; re-runs fail with
        // "duplicate column name", which is exactly the no-op we want.
        for ddl in [
            "ALTER TABLE runs ADD COLUMN commit_sha TEXT",
            "ALTER TABLE runs ADD COLUMN result_markdown TEXT",
            "ALTER TABLE runs ADD COLUMN cancel_requested INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE runs ADD COLUMN chat_session_id TEXT",
            "ALTER TABLE chat_sessions ADD COLUMN permission_mode TEXT",
            "ALTER TABLE chat_sessions ADD COLUMN reasoning_level TEXT",
            "ALTER TABLE chat_sessions ADD COLUMN archived INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE chat_sessions ADD COLUMN context_usage_json TEXT",
            "ALTER TABLE local_projects ADD COLUMN paper_id TEXT",
        ] {
            let _ = conn.execute(ddl, []);
        }
        // Older builds of this branch created a one-root-per-project unique
        // index; multiple baselines are allowed, so make sure it's gone.
        let _ = conn.execute(
            "DROP INDEX IF EXISTS uidx_local_experiments_project_baseline",
            [],
        );
        // Data migration: the chat_sessions.permission_mode wire ids were
        // neutralized off Claude Code's `--permission-mode` spelling (`default`,
        // `acceptEdits`, `bypassPermissions`) onto harness-agnostic ids (`ask`,
        // `accept-edits`, `bypass`) once Codex's sandbox policies stopped mapping
        // onto Claude's strings. Rewrite any rows written under the old scheme.
        // `plan`/`auto` were already harness-agnostic and need no rewrite.
        // Idempotent: after the first pass no old spellings remain to match.
        for (old, new) in [
            ("default", "ask"),
            ("acceptEdits", "accept-edits"),
            ("bypassPermissions", "bypass"),
        ] {
            let _ = conn.execute(
                "UPDATE chat_sessions SET permission_mode = ?2 WHERE permission_mode = ?1",
                params![old, new],
            );
        }
        // Retired permission modes → `auto`, per harness:
        //  * Claude Code KEEPS `plan` — it's a real mode again (the plan-gate
        //    hook + mcp-gate permission bridge make read-only planning and
        //    plan approval work headless). `ask`/`accept-edits` stay retired
        //    from the *picker* (never grantable headless mid-turn), and a
        //    session parked on them by an old build normalizes to `auto`.
        //    NOTE: this list runs on every open — a mode offered by
        //    `options()` must never appear in it, or picking that mode
        //    silently degrades to `auto` on the next request (exactly what
        //    happened to `plan` between #75 and this fix).
        //  * Codex KEEPS `plan` — it's a real mode now too (native
        //    collaboration mode over the app-server: the plan.md template,
        //    `request_user_input` question cards, and the streamed plan item
        //    make read-mostly planning and plan approval work). Only
        //    `ask`/`accept-edits` stay retired (never grantable). Same rule as
        //    Claude's `plan` above: a mode offered by `options()` must NEVER
        //    appear in this list, or picking it silently degrades to `auto` on
        //    the next request.
        //  * OpenCode dropped its hollow `ask` (its default is permissive, so a
        //    dedicated ask mode almost never fired) — but KEEPS `plan` (its real
        //    plan agent), so that one is left untouched.
        let _ = conn.execute(
            "UPDATE chat_sessions SET permission_mode = 'auto'
             WHERE (harness = 'claude-code'
                    AND permission_mode IN ('ask', 'accept-edits'))
                OR (harness = 'codex'
                    AND permission_mode IN ('ask', 'accept-edits'))
                OR (harness = 'opencode'
                    AND permission_mode IN ('ask', 'accept-edits'))",
            [],
        );
        Ok(Self { conn })
    }

    /// Short write transaction over this connection; rolls back when dropped
    /// without `commit()`. Keep network I/O out of the closure it guards.
    pub fn begin(&self) -> Result<rusqlite::Transaction<'_>> {
        Ok(self.conn.unchecked_transaction()?)
    }

    /// Coalesce the WAL back into the main `orx.db` file and truncate it, so a
    /// filesystem-level copy of `orx.db` alone captures all committed data.
    /// Best-effort — used before relocating the data dir. Errors are returned so
    /// the caller can decide, but a busy checkpoint is non-fatal (the WAL sidecar
    /// gets copied too when present).
    pub fn checkpoint(&self) -> Result<()> {
        self.conn
            .pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
        Ok(())
    }

    pub fn upsert_run(&self, run: &StoredRun) -> Result<()> {
        self.conn.execute(
            "INSERT INTO runs (id, experiment_id, project_id, status, backend_json, command,
                               created_at, updated_at, ended_at, exit_code,
                               commit_sha, result_markdown, cancel_requested,
                               chat_session_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
             ON CONFLICT(id) DO UPDATE SET
               status = excluded.status,
               backend_json = excluded.backend_json,
               updated_at = excluded.updated_at,
               ended_at = excluded.ended_at,
               exit_code = excluded.exit_code,
               commit_sha = excluded.commit_sha,
               result_markdown = excluded.result_markdown",
            // chat_session_id is deliberately absent from the DO UPDATE SET:
            // run ownership is immutable, so a later status upsert never
            // rewrites (or clears) the session that launched the run.
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
                run.commit_sha,
                run.result_markdown,
                run.cancel_requested,
                run.chat_session_id,
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

    /// Count runs in an active state (`starting`/`running`) — SQL-side and
    /// unbounded, so a long-running job older than the newest N rows still
    /// counts. Used by the data-dir move's in-flight guard.
    pub fn count_active_runs(&self) -> Result<usize> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM runs WHERE status IN ('starting', 'running')",
            [],
            |r| r.get(0),
        )?;
        Ok(n as usize)
    }

    pub fn list_runs_by_project(&self, project_id: &str) -> Result<Vec<StoredRun>> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_RUN} WHERE project_id = ?1 ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map(params![project_id], row_to_run)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    // Consumed by later local-mode stages (supervise + `orx up` API).
    #[allow(dead_code)]
    pub fn list_runs_by_experiment(&self, experiment_id: &str) -> Result<Vec<StoredRun>> {
        let mut stmt = self.conn.prepare(&format!(
            "{SELECT_RUN} WHERE experiment_id = ?1 ORDER BY created_at DESC"
        ))?;
        let rows = stmt.query_map(params![experiment_id], row_to_run)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn latest_run_for_experiment(&self, experiment_id: &str) -> Result<Option<StoredRun>> {
        let run = self
            .conn
            .query_row(
                &format!("{SELECT_RUN} WHERE experiment_id = ?1 ORDER BY created_at DESC LIMIT 1"),
                params![experiment_id],
                row_to_run,
            )
            .optional()?;
        Ok(run)
    }

    pub fn set_cancel_requested(&self, run_id: &str, requested: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET cancel_requested = ?2, updated_at = ?3 WHERE id = ?1",
            params![run_id, requested, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_result_markdown(&self, run_id: &str, markdown: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET result_markdown = ?2, updated_at = ?3 WHERE id = ?1",
            params![run_id, markdown, now_ms()],
        )?;
        Ok(())
    }

    /// Update only the run's backend descriptor — for a supervisor learning
    /// more about its job mid-flight (e.g. the openresearch box's SSH
    /// endpoint) without clobbering status/markdown/cancel state.
    pub fn set_backend_json(&self, run_id: &str, backend_json: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET backend_json = ?2, updated_at = ?3 WHERE id = ?1",
            params![run_id, backend_json, now_ms()],
        )?;
        Ok(())
    }

    // --- local projects (orx up) ---

    pub fn create_local_project(&self, p: &LocalProject) -> Result<()> {
        self.conn.execute(
            &format!("INSERT INTO local_projects ({PROJECT_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"),
            params![
                p.id, p.name, p.slug, p.github_owner, p.github_repo,
                p.baseline_branch, p.repo_path, p.run_command, p.paper_id, p.created_at, p.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_local_project(&self, id: &str) -> Result<Option<LocalProject>> {
        let p = self
            .conn
            .query_row(
                &format!("SELECT {PROJECT_COLS} FROM local_projects WHERE id = ?1"),
                params![id],
                LocalProject::from_row,
            )
            .optional()?;
        Ok(p)
    }

    #[allow(dead_code)]
    pub fn get_local_project_by_slug(&self, slug: &str) -> Result<Option<LocalProject>> {
        let p = self
            .conn
            .query_row(
                &format!("SELECT {PROJECT_COLS} FROM local_projects WHERE slug = ?1"),
                params![slug],
                LocalProject::from_row,
            )
            .optional()?;
        Ok(p)
    }

    pub fn list_local_projects(&self) -> Result<Vec<LocalProject>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {PROJECT_COLS} FROM local_projects ORDER BY updated_at DESC"
        ))?;
        let rows = stmt.query_map([], LocalProject::from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Delete a project and everything hanging off it (chats, runs,
    /// experiments) in one transaction. GitHub repo and cache clone are kept.
    pub fn delete_local_project(&self, id: &str) -> Result<()> {
        let tx = self.begin()?;
        self.conn.execute(
            "DELETE FROM chat_messages WHERE session_id IN
               (SELECT id FROM chat_sessions WHERE project_id = ?1)",
            params![id],
        )?;
        self.conn.execute(
            "DELETE FROM chat_sessions WHERE project_id = ?1",
            params![id],
        )?;
        self.conn
            .execute("DELETE FROM runs WHERE project_id = ?1", params![id])?;
        self.conn.execute(
            "DELETE FROM local_experiments WHERE project_id = ?1",
            params![id],
        )?;
        self.conn
            .execute("DELETE FROM local_projects WHERE id = ?1", params![id])?;
        tx.commit()?;
        Ok(())
    }

    /// Bump updated_at only — records a visit for the recency sort and fires
    /// the SSE project.updated diff.
    pub fn touch_local_project(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE local_projects SET updated_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    /// Full-row update by id (name / run_command / branch edits).
    pub fn update_local_project(&self, p: &LocalProject) -> Result<()> {
        self.conn.execute(
            "UPDATE local_projects SET name = ?2, slug = ?3, github_owner = ?4, github_repo = ?5,
                    baseline_branch = ?6, repo_path = ?7, run_command = ?8, paper_id = ?9,
                    updated_at = ?10
             WHERE id = ?1",
            params![
                p.id,
                p.name,
                p.slug,
                p.github_owner,
                p.github_repo,
                p.baseline_branch,
                p.repo_path,
                p.run_command,
                p.paper_id,
                now_ms(),
            ],
        )?;
        Ok(())
    }

    // --- local experiments (orx up) ---

    pub fn create_local_experiment(&self, e: &LocalExperiment) -> Result<()> {
        self.conn.execute(
            &format!("INSERT INTO local_experiments ({EXPERIMENT_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)"),
            params![
                e.id, e.project_id, e.parent_experiment_id, e.slug, e.branch_name,
                e.title, e.description, e.run_command, e.agent_status, e.created_at, e.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_local_experiment(&self, id: &str) -> Result<Option<LocalExperiment>> {
        let e = self
            .conn
            .query_row(
                &format!("SELECT {EXPERIMENT_COLS} FROM local_experiments WHERE id = ?1"),
                params![id],
                LocalExperiment::from_row,
            )
            .optional()?;
        Ok(e)
    }

    pub fn list_experiments_by_project(&self, project_id: &str) -> Result<Vec<LocalExperiment>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {EXPERIMENT_COLS} FROM local_experiments WHERE project_id = ?1 ORDER BY created_at ASC"
        ))?;
        let rows = stmt.query_map(params![project_id], LocalExperiment::from_row)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// Full-row update by id (title / description / run_command / agent_status).
    pub fn update_local_experiment(&self, e: &LocalExperiment) -> Result<()> {
        self.conn.execute(
            "UPDATE local_experiments SET parent_experiment_id = ?2, slug = ?3, branch_name = ?4,
                    title = ?5, description = ?6, run_command = ?7, agent_status = ?8, updated_at = ?9
             WHERE id = ?1",
            params![
                e.id, e.parent_experiment_id, e.slug, e.branch_name,
                e.title, e.description, e.run_command, e.agent_status, now_ms(),
            ],
        )?;
        Ok(())
    }

    // --- chat sessions / messages ------------------------------------------

    pub fn create_chat_session(&self, s: &StoredChatSession) -> Result<()> {
        self.conn.execute(
            "INSERT INTO chat_sessions (id, project_id, harness, native_session_id, title, model,
                                        permission_mode, reasoning_level, archived, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                s.id,
                s.project_id,
                s.harness,
                s.native_session_id,
                s.title,
                s.model,
                s.permission_mode,
                s.reasoning_level,
                s.archived,
                s.created_at,
                s.updated_at,
            ],
        )?;
        Ok(())
    }

    pub fn get_chat_session(&self, id: &str) -> Result<Option<StoredChatSession>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CHAT_SESSION_COLS} FROM chat_sessions WHERE id = ?1"
        ))?;
        let mut rows = stmt.query_map(params![id], row_to_chat_session)?;
        Ok(rows.next().transpose()?)
    }

    pub fn list_chat_sessions_by_project(
        &self,
        project_id: &str,
    ) -> Result<Vec<StoredChatSession>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CHAT_SESSION_COLS} FROM chat_sessions WHERE project_id = ?1
             ORDER BY updated_at DESC"
        ))?;
        let rows = stmt.query_map(params![project_id], row_to_chat_session)?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    pub fn delete_chat_session(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM chat_messages WHERE session_id = ?1",
            params![id],
        )?;
        self.conn
            .execute("DELETE FROM chat_sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn set_chat_session_native_id(&self, id: &str, native_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE chat_sessions SET native_session_id = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, native_id, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_chat_session_model(&self, id: &str, model: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE chat_sessions SET model = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, model, now_ms()],
        )?;
        Ok(())
    }

    /// Persist the latest context-window usage (serialized `ContextUsage`).
    /// Does not bump `updated_at` — usage is a passive by-product of a turn that
    /// already bumped it, and re-ordering the session on every token report would
    /// be noise.
    pub fn set_chat_session_context_usage(&self, id: &str, json: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE chat_sessions SET context_usage_json = ?2 WHERE id = ?1",
            params![id, json],
        )?;
        Ok(())
    }

    pub fn set_chat_session_permission_mode(&self, id: &str, mode: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE chat_sessions SET permission_mode = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, mode, now_ms()],
        )?;
        Ok(())
    }

    pub fn set_chat_session_reasoning_level(&self, id: &str, level: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE chat_sessions SET reasoning_level = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, level, now_ms()],
        )?;
        Ok(())
    }

    /// Archive/unarchive. Doesn't bump `updated_at`, so the session keeps its
    /// place in the recency ordering when it comes back.
    pub fn set_chat_session_archived(&self, id: &str, archived: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE chat_sessions SET archived = ?2 WHERE id = ?1",
            params![id, archived],
        )?;
        Ok(())
    }

    pub fn set_chat_session_title(&self, id: &str, title: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE chat_sessions SET title = ?2, updated_at = ?3 WHERE id = ?1",
            params![id, title, now_ms()],
        )?;
        Ok(())
    }

    /// Set the title only if the session currently has none (NULL or blank).
    /// Atomic check-and-set for harness auto-titling, so it can't clobber a
    /// title the user set via Rename. Returns true if a row was written.
    pub fn set_chat_session_title_if_empty(&self, id: &str, title: &str) -> Result<bool> {
        let n = self.conn.execute(
            "UPDATE chat_sessions SET title = ?2, updated_at = ?3 \
             WHERE id = ?1 AND (title IS NULL OR trim(title) = '')",
            params![id, title, now_ms()],
        )?;
        Ok(n > 0)
    }

    pub fn touch_chat_session(&self, id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE chat_sessions SET updated_at = ?2 WHERE id = ?1",
            params![id, now_ms()],
        )?;
        Ok(())
    }

    /// Insert or replace a message's parts — assistant messages are rewritten
    /// as their parts stream in.
    pub fn upsert_chat_message(&self, m: &StoredChatMessage) -> Result<()> {
        self.conn.execute(
            "INSERT INTO chat_messages (id, session_id, role, parts_json, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(id) DO UPDATE SET parts_json = excluded.parts_json",
            params![m.id, m.session_id, m.role, m.parts_json, m.created_at],
        )?;
        Ok(())
    }

    pub fn list_chat_messages(&self, session_id: &str) -> Result<Vec<StoredChatMessage>> {
        let mut stmt = self.conn.prepare(
            // rowid tiebreak: a user message and its reply can share a millisecond.
            "SELECT id, session_id, role, parts_json, created_at FROM chat_messages
             WHERE session_id = ?1 ORDER BY created_at ASC, rowid ASC",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(StoredChatMessage {
                id: row.get(0)?,
                session_id: row.get(1)?,
                role: row.get(2)?,
                parts_json: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }

    /// A single chat message by id (used to reconcile a message's persisted
    /// state against an in-memory copy mid-turn). `None` if it doesn't exist.
    pub fn get_chat_message(&self, id: &str) -> Result<Option<StoredChatMessage>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, session_id, role, parts_json, created_at FROM chat_messages
                 WHERE id = ?1",
                params![id],
                |row| {
                    Ok(StoredChatMessage {
                        id: row.get(0)?,
                        session_id: row.get(1)?,
                        role: row.get(2)?,
                        parts_json: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                },
            )
            .optional()?)
    }

    pub fn upsert_ssh_host_test(&self, t: &SshHostTest) -> Result<()> {
        self.conn.execute(
            "INSERT INTO ssh_host_tests (host, reachable, git_found, error, tested_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(host) DO UPDATE SET
               reachable = excluded.reachable,
               git_found = excluded.git_found,
               error = excluded.error,
               tested_at = excluded.tested_at",
            params![t.host, t.reachable, t.git_found, t.error, t.tested_at],
        )?;
        Ok(())
    }

    pub fn list_ssh_host_tests(&self) -> Result<Vec<SshHostTest>> {
        let mut stmt = self
            .conn
            .prepare("SELECT host, reachable, git_found, error, tested_at FROM ssh_host_tests")?;
        let rows = stmt.query_map([], |row| {
            Ok(SshHostTest {
                host: row.get(0)?,
                reachable: row.get(1)?,
                git_found: row.get(2)?,
                error: row.get(3)?,
                tested_at: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<Vec<_>, _>>()?)
    }
}

/// Most recent preflight result per ssh host alias (Settings → Compute → SSH).
/// Serializes to the wire shape the UI's `SshPreflight` type expects; `host`
/// is the row key only (the API embeds results under their host entry).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SshHostTest {
    #[serde(skip_serializing)]
    pub host: String,
    pub reachable: bool,
    pub git_found: bool,
    pub error: Option<String>,
    /// Unix millis.
    pub tested_at: i64,
}

/// One chat thread with a harness. `native_session_id` is the harness's own
/// session/rollout id (set after the first turn for CLIs that mint it lazily).
#[derive(Debug, Clone)]
pub struct StoredChatSession {
    pub id: String,
    pub project_id: String,
    pub harness: String,
    pub native_session_id: Option<String>,
    pub title: Option<String>,
    pub model: Option<String>,
    /// Permission-mode wire id (`"auto"` / `"plan"` / …); None = harness default.
    pub permission_mode: Option<String>,
    /// Reasoning-level wire id (`"low"` / `"medium"` / `"high"`); None = default.
    pub reasoning_level: Option<String>,
    /// Hidden from the default Recents list, but fully intact and resumable.
    pub archived: bool,
    /// Serialized `ContextUsage` for the latest turn; None until first reported.
    pub context_usage_json: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Normalized transcript entry; `parts_json` is the wire-format parts array
/// the UI renders (orx is the system of record for transcripts, not the
/// harness's own storage).
#[derive(Debug, Clone)]
pub struct StoredChatMessage {
    pub id: String,
    pub session_id: String,
    pub role: String,
    pub parts_json: String,
    pub created_at: i64,
}

const CHAT_SESSION_COLS: &str = "id, project_id, harness, native_session_id, title, model, \
     permission_mode, reasoning_level, archived, context_usage_json, created_at, updated_at";

fn row_to_chat_session(
    row: &rusqlite::Row<'_>,
) -> std::result::Result<StoredChatSession, rusqlite::Error> {
    Ok(StoredChatSession {
        id: row.get(0)?,
        project_id: row.get(1)?,
        harness: row.get(2)?,
        native_session_id: row.get(3)?,
        title: row.get(4)?,
        model: row.get(5)?,
        permission_mode: row.get(6)?,
        reasoning_level: row.get(7)?,
        archived: row.get(8)?,
        context_usage_json: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
    })
}

const SELECT_RUN: &str = "SELECT id, experiment_id, project_id, status, backend_json, command,
                                 created_at, updated_at, ended_at, exit_code,
                                 commit_sha, result_markdown, cancel_requested,
                                 chat_session_id FROM runs";

const PROJECT_COLS: &str = "id, name, slug, github_owner, github_repo, baseline_branch, \
                            repo_path, run_command, paper_id, created_at, updated_at";

const EXPERIMENT_COLS: &str = "id, project_id, parent_experiment_id, slug, branch_name, \
                               title, description, run_command, agent_status, created_at, updated_at";

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
        commit_sha: row.get(10)?,
        result_markdown: row.get(11)?,
        cancel_requested: row.get(12)?,
        chat_session_id: row.get(13)?,
    })
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn human_bytes_scales() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
    }

    #[test]
    fn chat_session_context_usage_roundtrips() {
        let dir = std::env::temp_dir().join(format!("orx-store-ctxusage-{}", uuid::Uuid::new_v4()));
        let store = Store::open_at(dir.clone()).unwrap();
        let session = StoredChatSession {
            id: "chat_1".into(),
            project_id: "proj_1".into(),
            harness: "claude-code".into(),
            native_session_id: None,
            title: None,
            model: Some("claude-haiku-4-5".into()),
            permission_mode: None,
            reasoning_level: None,
            archived: false,
            context_usage_json: None,
            created_at: 1,
            updated_at: 1,
        };
        store.create_chat_session(&session).unwrap();
        // Fresh session: no usage yet.
        assert!(store
            .get_chat_session("chat_1")
            .unwrap()
            .unwrap()
            .context_usage_json
            .is_none());
        // Set, then read it back verbatim.
        let json = r#"{"usedTokens":27564,"contextWindow":200000}"#;
        store
            .set_chat_session_context_usage("chat_1", json)
            .unwrap();
        assert_eq!(
            store
                .get_chat_session("chat_1")
                .unwrap()
                .unwrap()
                .context_usage_json
                .as_deref(),
            Some(json)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn run_fixture(id: &str, status: &str, chat_session_id: Option<&str>) -> StoredRun {
        StoredRun {
            id: id.into(),
            experiment_id: "exp_1".into(),
            project_id: "proj_1".into(),
            status: status.into(),
            backend_json: "{}".into(),
            command: "echo hi".into(),
            created_at: 1,
            updated_at: 1,
            ended_at: None,
            exit_code: None,
            commit_sha: None,
            result_markdown: None,
            cancel_requested: false,
            chat_session_id: chat_session_id.map(str::to_string),
        }
    }

    #[test]
    fn run_chat_session_id_roundtrips() {
        let dir = std::env::temp_dir().join(format!("orx-store-runsess-{}", uuid::Uuid::new_v4()));
        let store = Store::open_at(dir.clone()).unwrap();

        store
            .upsert_run(&run_fixture("run_owned", "starting", Some("chat_A")))
            .unwrap();
        store
            .upsert_run(&run_fixture("run_orphan", "starting", None))
            .unwrap();

        assert_eq!(
            store.get_run("run_owned").unwrap().unwrap().chat_session_id,
            Some("chat_A".to_string())
        );
        assert_eq!(
            store
                .get_run("run_orphan")
                .unwrap()
                .unwrap()
                .chat_session_id,
            None
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn run_ownership_is_immutable_across_upserts() {
        let dir = std::env::temp_dir().join(format!("orx-store-runimmut-{}", uuid::Uuid::new_v4()));
        let store = Store::open_at(dir.clone()).unwrap();

        // Created by chat_A.
        store
            .upsert_run(&run_fixture("run_1", "starting", Some("chat_A")))
            .unwrap();
        // A later status upsert that carries a *different* (or absent) session
        // must NOT rewrite the owner — ownership is immutable.
        store
            .upsert_run(&run_fixture("run_1", "failed", Some("chat_B")))
            .unwrap();
        store
            .upsert_run(&run_fixture("run_1", "done", None))
            .unwrap();

        let run = store.get_run("run_1").unwrap().unwrap();
        assert_eq!(run.status, "done", "status still updates on conflict");
        assert_eq!(
            run.chat_session_id,
            Some("chat_A".to_string()),
            "the launching session is never overwritten by a later upsert"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
