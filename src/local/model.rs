//! Wire-friendly local-mode entities — the same camelCase shapes the `orx up`
//! HTTP API serves. Row conversions live here beside the structs; the SQL
//! (matching column order) lives in `store.rs`.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalProject {
    pub id: String,
    pub name: String,
    pub slug: String,
    /// Legacy database column retained only to migrate old worktree locations.
    #[serde(skip)]
    pub github_owner: String,
    #[serde(skip)]
    pub github_repo: String,
    #[serde(skip)]
    pub github_sync_enabled: bool,
    /// Fork point for baseline roots and the clone's default checkout — not
    /// where any experiment lives (legacy roots predating per-baseline
    /// branches may still ride it).
    pub baseline_branch: String,
    /// Local repository path.
    pub repo_path: String,
    pub run_command: Option<String>,
    /// arXiv id the project starts from (versionless, e.g. `2401.12345`).
    pub paper_id: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl LocalProject {
    pub fn has_github_repository(&self) -> bool {
        !self.github_owner.trim().is_empty() && !self.github_repo.trim().is_empty()
    }

    /// Column order must match `store::PROJECT_COLS`.
    pub(crate) fn from_row(row: &rusqlite::Row<'_>) -> std::result::Result<Self, rusqlite::Error> {
        Ok(Self {
            id: row.get(0)?,
            name: row.get(1)?,
            slug: row.get(2)?,
            github_owner: row.get(3)?,
            github_repo: row.get(4)?,
            github_sync_enabled: row.get(5)?,
            baseline_branch: row.get(6)?,
            repo_path: row.get(7)?,
            run_command: row.get(8)?,
            paper_id: row.get(9)?,
            created_at: row.get(10)?,
            updated_at: row.get(11)?,
        })
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalExperiment {
    pub id: String,
    pub project_id: String,
    /// NULL = baseline/root.
    pub parent_experiment_id: Option<String>,
    pub slug: String,
    /// `orx/<slug>` (legacy baselines ride the project's baseline branch).
    pub branch_name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub run_command: String,
    pub agent_status: String,
    pub created_at: i64,
    pub updated_at: i64,
    /// Chat session that created this experiment. NULL for dashboard-created,
    /// legacy and out-of-session rows. Immutable once stamped.
    pub chat_session_id: Option<String>,
}

impl LocalExperiment {
    /// Column order must match `store::EXPERIMENT_COLS`.
    pub(crate) fn from_row(row: &rusqlite::Row<'_>) -> std::result::Result<Self, rusqlite::Error> {
        Ok(Self {
            id: row.get(0)?,
            project_id: row.get(1)?,
            parent_experiment_id: row.get(2)?,
            slug: row.get(3)?,
            branch_name: row.get(4)?,
            title: row.get(5)?,
            description: row.get(6)?,
            run_command: row.get(7)?,
            agent_status: row.get(8)?,
            created_at: row.get(9)?,
            updated_at: row.get(10)?,
            chat_session_id: row.get(11)?,
        })
    }

    /// Display name: title when set, slug otherwise.
    pub fn display_name(&self) -> &str {
        match self.title.as_deref() {
            Some(t) if !t.trim().is_empty() => t,
            _ => &self.slug,
        }
    }
}
