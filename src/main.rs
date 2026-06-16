//! OpenResearch CLI (`orx`) — Rust port entry point.
//!
//! A clap-derive command tree mirroring the USAGE
//! block, dispatched from an async `tokio::main`. Each subcommand routes to one
//! module fn in `commands::<name>`. The six fs verbs (read/write/str-replace/
//! ls/grep/rm) all route into `commands::fs`.
//!
//! Error handling: command fns return `anyhow::Result<()>`. `main` prints the
//! error's `Display` to stderr and exits 1 — matching the TS
//! `main().catch(err => { console.error(err.message); process.exit(1) })`.

mod browser;
// DTOs faithfully mirror every API wire field; not all are read by the CLI yet.
#[allow(dead_code)]
mod client;
mod commands;
mod config;
mod error;
mod output;
mod updates;

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "orx",
    about = "OpenResearch CLI",
    version,
    disable_help_subcommand = true
)]
struct Cli {
    // Optional so a bare `orx` prints USAGE to stdout and exits 0 (like the TS
    // `if (!command) { console.log(USAGE); return; }`) instead of clap's exit-2.
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Log in via the browser and store a token.
    Login(LoginArgs),

    /// Remove the stored token.
    Logout,

    /// List your projects, grouped by organization.
    Projects(ProjectsArgs),

    /// List a project's experiments as a tree.
    Experiments(ExperimentsArgs),

    /// List the names (not values) of a project's environment variables.
    Env(EnvArgs),

    /// List a project's runs.
    Runs(RunsArgs),

    /// Read a run's terminal log (tail by default).
    Logs(LogsArgs),

    /// Grep run logs for a literal pattern.
    #[command(name = "search-logs")]
    SearchLogs(SearchLogsArgs),

    /// List the text artifacts a run produced (key + size).
    Artifacts(ArtifactsArgs),

    /// Read a run's text artifact (also caches it for SQL search).
    Artifact(ArtifactArgs),

    /// List the W&B runs linked to a run.
    Wandb(WandbArgs),

    /// Run read-only SQL against the project's evidence.
    Query(QueryArgs),

    /// Render a W&B metric across runs to a PNG.
    Chart(ChartArgs),

    /// Create a project (from a GitHub repo, or a fresh blank repo).
    #[command(name = "create-project")]
    CreateProject(CreateProjectArgs),

    /// Add an experiment node (child of a parent, or a baseline root).
    #[command(name = "create-experiment")]
    CreateExperiment(CreateExperimentArgs),

    /// List the GPU compute catalog.
    Compute(ComputeArgs),

    /// Operate on one experiment node (status / run command / run / cancel).
    Exp(ExpArgs),

    /// Upload or list a project's research reports (markdown + images).
    Report(ReportArgs),

    /// Print CLI usage for agents, or fetch a skill doc.
    Skill(SkillArgs),

    /// Install the OpenResearch skill into local coding agents (Claude Code, Codex, OpenCode, Cursor).
    #[command(name = "install-skills")]
    InstallSkills(InstallSkillsArgs),

    /// Search alphaXiv literature by full-text query (no login required).
    Lit(LitArgs),

    /// Fetch a paper's machine-readable report (or `--full` text) from alphaXiv.
    Paper(PaperArgs),

    /// Show the CLI version; `--check` compares it to the latest release.
    Version(VersionArgs),

    /// Update orx to the latest release (installer-script installs only).
    Update(UpdateArgs),
}

#[derive(Args, Debug)]
pub struct LoginArgs {
    /// Override the API base URL (or set OPENRESEARCH_API_URL).
    #[arg(long = "api-url")]
    pub api_url: Option<String>,
}

#[derive(Args, Debug)]
pub struct ProjectsArgs {
    /// Include archived projects.
    #[arg(long)]
    pub all: bool,
}

#[derive(Args, Debug)]
pub struct ExperimentsArgs {
    pub project_id: String,
}

#[derive(Args, Debug)]
pub struct EnvArgs {
    pub project_id: String,
}

#[derive(Args, Debug)]
pub struct RunsArgs {
    pub project_id: String,
    /// Filter to one experiment.
    #[arg(long)]
    pub experiment: Option<String>,
}

#[derive(Args, Debug)]
pub struct LogsArgs {
    pub run_id: String,
    /// Read from the start instead of the tail.
    #[arg(long)]
    pub head: bool,
    /// Max bytes to read.
    #[arg(long)]
    pub bytes: Option<String>,
    /// Exact byte window `<start>:<end>`.
    #[arg(long)]
    pub range: Option<String>,
}

#[derive(Args, Debug)]
pub struct SearchLogsArgs {
    pub project_id: String,
    pub pattern: String,
    /// Scope to a single run.
    #[arg(long)]
    pub run: Option<String>,
    /// Scope to a single experiment.
    #[arg(long)]
    pub experiment: Option<String>,
    /// Cap matching lines.
    #[arg(long)]
    pub max: Option<String>,
}

#[derive(Args, Debug)]
pub struct ArtifactsArgs {
    pub run_id: String,
}

#[derive(Args, Debug)]
pub struct WandbArgs {
    pub run_id: String,
}

#[derive(Args, Debug)]
pub struct ArtifactArgs {
    pub run_id: String,
    pub key: String,
    /// Read from the start instead of the tail.
    #[arg(long)]
    pub head: bool,
    /// Max bytes to read.
    #[arg(long)]
    pub bytes: Option<String>,
}

#[derive(Args, Debug)]
pub struct QueryArgs {
    pub project_id: String,
    pub sql: String,
}

#[derive(Args, Debug)]
pub struct ChartArgs {
    /// Chart kind. Only `wandb` is supported today.
    pub kind: String,
    pub project_id: String,
    /// W&B history key to plot.
    #[arg(long)]
    pub metric: Option<String>,
    /// Run to overlay (`<id>[:label]`); repeat for multiple runs.
    #[arg(long = "run")]
    pub run: Vec<String>,
    /// EMA smoothing 0–0.99.
    #[arg(long)]
    pub smoothing: Option<String>,
    /// Directory to save the rendered PNG.
    #[arg(long)]
    pub out: Option<String>,
}

#[derive(Args, Debug)]
pub struct CreateProjectArgs {
    /// Organization id (from `orx projects`).
    pub org_id: String,
    /// Project name (required).
    #[arg(long)]
    pub name: Option<String>,
    /// GitHub repo `owner/repo` (or github.com URL) to bind the project to.
    /// Omit to start the project on a fresh blank repo.
    #[arg(long)]
    pub repo: Option<String>,
    /// Branch the baseline imports from (with `--repo`; defaults to the repo's
    /// default branch).
    #[arg(long)]
    pub branch: Option<String>,
    /// Project description.
    #[arg(long)]
    pub description: Option<String>,
}

#[derive(Args, Debug)]
pub struct CreateExperimentArgs {
    pub project_id: String,
    /// Experiment title (required).
    #[arg(long)]
    pub title: Option<String>,
    /// Experiment description.
    #[arg(long)]
    pub description: Option<String>,
    /// Parent experiment id -> create a child. Omit to create a baseline on the
    /// project's bound repo.
    #[arg(long)]
    pub parent: Option<String>,
}

#[derive(Args, Debug)]
pub struct ComputeArgs {
    /// List CPU-only instance offers instead of the GPU catalog. CPU instances
    /// suit GPU-less experiments (data prep, eval harnesses, CPU-bound papers).
    #[arg(long)]
    pub cpu: bool,
    /// Filter to one GPU id (e.g. `H100_SXM`). Case-insensitive. GPU mode only.
    #[arg(long)]
    pub gpu: Option<String>,
    /// Filter to a specific GPU count per instance. GPU mode only.
    #[arg(long)]
    pub count: Option<i64>,
}

#[derive(Args, Debug)]
pub struct ExpArgs {
    #[command(subcommand)]
    pub command: ExpCommand,
}

#[derive(Subcommand, Debug)]
pub enum ExpCommand {
    /// Show the experiment's status, run command, and latest run.
    Status { exp_id: String },

    /// View the run command, or set it with `--set`.
    Cmd {
        exp_id: String,
        /// Set the run command to this value.
        #[arg(long)]
        set: Option<String>,
    },

    /// View the experiment's description/notes, or overwrite it with `--set` / `--stdin`.
    Desc {
        exp_id: String,
        /// Overwrite the description with this value.
        #[arg(long)]
        set: Option<String>,
        /// Overwrite the description with the whole of stdin (for long markdown docs).
        #[arg(long)]
        stdin: bool,
    },

    /// Launch a run on new (`--gpu`) or existing (`--sandbox`) compute.
    Run(ExpRunArgs),

    /// Cancel the in-flight run.
    Cancel { exp_id: String },

    /// Wait for a run to finish: one experiment (`<expId>`) or the next completion in a project (`--project`).
    Wait {
        /// Experiment to watch; its latest run is polled until it reaches a
        /// terminal state. Omit and pass `--project` to watch a whole project.
        exp_id: Option<String>,
        /// Watch every run in this project and return on the FIRST one to
        /// complete (reach done/failed/cancelled) — a "slot freed" signal. Call
        /// it in a loop, re-listing `orx runs` on each return to catch all
        /// finished runs. Returns immediately ("drained: no runs in flight") if
        /// none are in flight. Mutually exclusive with `<expId>`.
        #[arg(long)]
        project: Option<String>,
        /// Give up and exit non-zero after this many seconds (default 1800).
        #[arg(long)]
        timeout: Option<u64>,
        /// Seconds between polls (default 5).
        #[arg(long)]
        interval: Option<u64>,
    },
}

#[derive(Args, Debug)]
pub struct ReportArgs {
    #[command(subcommand)]
    pub command: ReportCommand,
}

#[derive(Subcommand, Debug)]
pub enum ReportCommand {
    /// Upload a report folder (report.md + images/) to a project.
    Upload {
        project_id: String,
        /// Path to the report folder on disk.
        folder: String,
        /// Report title (defaults to the folder name).
        #[arg(long)]
        title: Option<String>,
    },

    /// List a project's reports.
    List { project_id: String },
}

#[derive(Args, Debug)]
pub struct ExpRunArgs {
    pub exp_id: String,
    /// Provision a new instance with this GPU id, e.g. `H100_SXM` — the exact
    /// id from `orx compute`, not a family name like `H100`.
    #[arg(long)]
    pub gpu: Option<String>,
    /// GPUs per instance (with `--gpu`; default 1).
    #[arg(long)]
    pub count: Option<i64>,
    /// Disk in GB (with `--gpu`; default 100).
    #[arg(long)]
    pub disk: Option<i64>,
    /// Provision a CPU-only instance with this flavor: cpu5c (compute), cpu5g
    /// (general), or cpu5m (memory-optimized). Mutually exclusive with `--gpu`.
    #[arg(long)]
    pub cpu: Option<String>,
    /// vCPUs for a CPU instance (with `--cpu`): 2, 8, or 32 (default 8).
    #[arg(long)]
    pub vcpus: Option<i64>,
    /// Run on an existing sandbox instead of provisioning. Mutually exclusive with `--gpu`/`--cpu`.
    #[arg(long)]
    pub sandbox: Option<String>,
    /// Launch even if the experiment's branch has no changes over its parent
    /// (bypasses the "did you forget to push?" guard, for a deliberate re-run).
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct SkillArgs {
    pub path: Option<String>,
}

#[derive(Args, Debug)]
pub struct InstallSkillsArgs {
    /// Which agent(s) to install into: `claude`, `codex`, `opencode`, `cursor`,
    /// or `all`. Defaults to every agent already set up on this machine.
    #[arg(long)]
    pub agent: Option<String>,
}

#[derive(Args, Debug)]
pub struct LitArgs {
    /// Full-text search query.
    pub query: String,
    /// Max results (default 5).
    #[arg(long)]
    pub limit: Option<u32>,
    /// Emit raw JSON instead of the formatted list.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct VersionArgs {
    /// Also check the latest released version on GitHub.
    #[arg(long)]
    pub check: bool,
    /// Emit a JSON object instead of text (implies --check).
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct UpdateArgs {
    /// Report whether an update is available without installing anything.
    #[arg(long)]
    pub dry_run: bool,
    /// Update even when the binary doesn't match the install receipt
    /// (multiple copies, or a `cargo install` overwrote it).
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct PaperArgs {
    /// arXiv id, versioned id (`2401.12345v2`), or an arXiv/alphaXiv URL.
    pub id: String,
    /// Fetch the full extracted paper text instead of the report.
    #[arg(long)]
    pub full: bool,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        // Bare `orx`: print the command overview to stdout and exit 0.
        use clap::CommandFactory;
        Cli::command().print_help().ok();
        return;
    };
    // Passive update nudge (skipped for the commands that manage updates
    // themselves). Interactive terminals only; never delays or fails the
    // command, never touches stdout or the exit code.
    let nudge =
        (!matches!(command, Command::Version(_) | Command::Update(_))).then(updates::Nudge::start);

    let result = dispatch(command).await;
    if let Some(nudge) = nudge {
        nudge.finish().await;
    }

    if let Err(err) = result {
        // Match the TS: print only the message, exit 1.
        eprintln!("{}", err);
        std::process::exit(1);
    }
}

async fn dispatch(command: Command) -> error::Result<()> {
    match command {
        Command::Login(args) => commands::login::run(args).await,
        Command::Logout => commands::logout::run().await,
        Command::Projects(args) => commands::projects::run(args).await,
        Command::Experiments(args) => commands::experiments::run(args).await,
        Command::Env(args) => commands::env::run(args).await,
        Command::Runs(args) => commands::runs::run(args).await,
        Command::Logs(args) => commands::logs::run(args).await,
        Command::SearchLogs(args) => commands::search_logs::run(args).await,
        Command::Artifacts(args) => commands::artifacts::run(args).await,
        Command::Artifact(args) => commands::artifact::run(args).await,
        Command::Wandb(args) => commands::wandb::run(args).await,
        Command::Query(args) => commands::query::run(args).await,
        Command::Chart(args) => commands::chart::run(args).await,
        Command::CreateProject(args) => commands::create_project::run(args).await,
        Command::CreateExperiment(args) => commands::create_experiment::run(args).await,
        Command::Compute(args) => commands::compute::run(args).await,
        Command::Exp(args) => commands::exp::run(args).await,
        Command::Report(args) => commands::report::run(args).await,
        Command::Skill(args) => commands::skill::run(args).await,
        Command::InstallSkills(args) => commands::install_skills::run(args).await,
        Command::Lit(args) => commands::lit::run(args).await,
        Command::Paper(args) => commands::paper::run(args).await,
        Command::Version(args) => commands::version::run(args).await,
        Command::Update(args) => commands::update::run(args).await,
    }
}
