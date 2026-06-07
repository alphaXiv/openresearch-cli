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

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "orx",
    about = "OpenResearch CLI",
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

    /// List a project's runs.
    Runs(RunsArgs),

    /// Read a run's terminal log (tail by default).
    Logs(LogsArgs),

    /// Grep run logs for a literal pattern.
    #[command(name = "search-logs")]
    SearchLogs(SearchLogsArgs),

    /// Grep an experiment's committed branch (no dev node).
    Search(SearchArgs),

    /// List committed files (no dev node).
    Tree(TreeArgs),

    /// Read a committed file (no dev node).
    Cat(CatArgs),

    /// List the text artifacts a run produced (key + size).
    Artifacts(ArtifactsArgs),

    /// Read a run's text artifact (also caches it for SQL search).
    Artifact(ArtifactArgs),

    /// List the W&B runs linked to a run.
    Wandb(WandbArgs),

    /// Show a run's cumulative code diff vs. its parent branch.
    Diff(DiffArgs),

    /// Run read-only SQL against the project's evidence.
    Query(QueryArgs),

    /// Render a W&B metric across runs to a PNG.
    Chart(ChartArgs),

    /// Add an experiment node (child, git-repo root, or empty root).
    #[command(name = "create-experiment")]
    CreateExperiment(CreateExperimentArgs),

    /// List the GPU compute catalog.
    Compute(ComputeArgs),

    /// Operate on one experiment node (status / run command / run / cancel).
    Exp(ExpArgs),

    /// Provision / inspect / tear down a dev node.
    Dev(DevArgs),

    // ---- fs verbs: all dispatch to commands::fs ----
    /// Read a file from the dev working tree.
    Read(FsReadArgs),

    /// Write a file (content on stdin).
    Write(FsWriteArgs),

    /// Replace an exact unique snippet.
    #[command(name = "str-replace")]
    StrReplace(FsStrReplaceArgs),

    /// List files.
    Ls(FsLsArgs),

    /// Search files.
    Grep(FsGrepArgs),

    /// Delete a file.
    Rm(FsRmArgs),

    /// Print CLI usage for agents, or fetch a skill doc.
    Skill(SkillArgs),
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
pub struct SearchArgs {
    pub exp_id: String,
    pub query: String,
}

#[derive(Args, Debug)]
pub struct TreeArgs {
    pub exp_id: String,
    pub path: Option<String>,
}

#[derive(Args, Debug)]
pub struct CatArgs {
    pub exp_id: String,
    pub path: String,
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
pub struct DiffArgs {
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
pub struct CreateExperimentArgs {
    pub project_id: String,
    /// Experiment title (required).
    #[arg(long)]
    pub title: Option<String>,
    /// Experiment description.
    #[arg(long)]
    pub description: Option<String>,
    /// Parent experiment id -> create a child.
    #[arg(long)]
    pub parent: Option<String>,
    /// GitHub repo `owner/repo` -> create a root from it.
    #[arg(long)]
    pub repo: Option<String>,
    /// Branch/tag/commit for `--repo`.
    #[arg(long)]
    pub ref_: Option<String>,
}

#[derive(Args, Debug)]
pub struct ComputeArgs {
    /// Filter to one GPU id (e.g. `H100`). Case-insensitive.
    #[arg(long)]
    pub gpu: Option<String>,
    /// Filter to a specific GPU count per instance.
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

    /// Block until a run finishes (`<expId>`) or any run in a project changes (`--project`).
    Wait {
        /// Experiment to watch; its latest run is polled until it reaches a
        /// terminal state. Omit and pass `--project` to watch a whole project.
        exp_id: Option<String>,
        /// Watch every run in this project and return on the first change of any
        /// kind (new run, status transition). Mutually exclusive with `<expId>`.
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
pub struct ExpRunArgs {
    pub exp_id: String,
    /// Provision a new instance with this GPU id (see `orx compute`).
    #[arg(long)]
    pub gpu: Option<String>,
    /// GPUs per instance (with `--gpu`; default 1).
    #[arg(long)]
    pub count: Option<i64>,
    /// Disk in GB (with `--gpu`; default 100).
    #[arg(long)]
    pub disk: Option<i64>,
    /// Run on an existing sandbox instead of provisioning. Mutually exclusive with `--gpu`.
    #[arg(long)]
    pub sandbox: Option<String>,
}

#[derive(Args, Debug)]
pub struct DevArgs {
    /// `open`, `close`, or `status`.
    pub action: String,
    pub exp_id: String,
    /// Commit message (close).
    #[arg(short = 'm', long = "message")]
    pub message: Option<String>,
    /// Discard the session without committing (close).
    #[arg(long)]
    pub discard: bool,
}

#[derive(Args, Debug)]
pub struct FsReadArgs {
    pub exp_id: String,
    pub path: String,
}

#[derive(Args, Debug)]
pub struct FsWriteArgs {
    pub exp_id: String,
    pub path: String,
}

#[derive(Args, Debug)]
pub struct FsStrReplaceArgs {
    pub exp_id: String,
    pub path: String,
    pub old_string: String,
    pub new_string: String,
}

#[derive(Args, Debug)]
pub struct FsLsArgs {
    pub exp_id: String,
    pub path: Option<String>,
}

#[derive(Args, Debug)]
pub struct FsGrepArgs {
    pub exp_id: String,
    pub pattern: String,
}

#[derive(Args, Debug)]
pub struct FsRmArgs {
    pub exp_id: String,
    pub path: String,
}

#[derive(Args, Debug)]
pub struct SkillArgs {
    pub path: Option<String>,
}

/// Which fs verb is being invoked. Passed to `commands::fs::run` so the single
/// module can build the right `DevFsOp`, mirroring the TS `fsCommand(verb, ...)`.
#[derive(Debug, Clone, Copy)]
pub enum FsVerb {
    Read,
    Write,
    StrReplace,
    Ls,
    Grep,
    Rm,
}

/// Normalized fs invocation handed to `commands::fs::run`.
#[derive(Debug)]
pub struct FsInvocation {
    pub verb: FsVerb,
    pub exp_id: String,
    /// Positional args after the experiment id (path, pattern, old/new, ...).
    pub rest: Vec<String>,
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
    if let Err(err) = dispatch(command).await {
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
        Command::Runs(args) => commands::runs::run(args).await,
        Command::Logs(args) => commands::logs::run(args).await,
        Command::SearchLogs(args) => commands::search_logs::run(args).await,
        Command::Search(args) => commands::search::run(args).await,
        Command::Tree(args) => commands::tree::run(args).await,
        Command::Cat(args) => commands::cat::run(args).await,
        Command::Artifacts(args) => commands::artifacts::run(args).await,
        Command::Artifact(args) => commands::artifact::run(args).await,
        Command::Wandb(args) => commands::wandb::run(args).await,
        Command::Diff(args) => commands::diff::run(args).await,
        Command::Query(args) => commands::query::run(args).await,
        Command::Chart(args) => commands::chart::run(args).await,
        Command::CreateExperiment(args) => commands::create_experiment::run(args).await,
        Command::Compute(args) => commands::compute::run(args).await,
        Command::Exp(args) => commands::exp::run(args).await,
        Command::Dev(args) => commands::dev::run(args).await,
        Command::Read(a) => {
            commands::fs::run(FsInvocation {
                verb: FsVerb::Read,
                exp_id: a.exp_id,
                rest: vec![a.path],
            })
            .await
        }
        Command::Write(a) => {
            commands::fs::run(FsInvocation {
                verb: FsVerb::Write,
                exp_id: a.exp_id,
                rest: vec![a.path],
            })
            .await
        }
        Command::StrReplace(a) => {
            commands::fs::run(FsInvocation {
                verb: FsVerb::StrReplace,
                exp_id: a.exp_id,
                rest: vec![a.path, a.old_string, a.new_string],
            })
            .await
        }
        Command::Ls(a) => {
            let rest = a.path.map(|p| vec![p]).unwrap_or_default();
            commands::fs::run(FsInvocation {
                verb: FsVerb::Ls,
                exp_id: a.exp_id,
                rest,
            })
            .await
        }
        Command::Grep(a) => {
            commands::fs::run(FsInvocation {
                verb: FsVerb::Grep,
                exp_id: a.exp_id,
                rest: vec![a.pattern],
            })
            .await
        }
        Command::Rm(a) => {
            commands::fs::run(FsInvocation {
                verb: FsVerb::Rm,
                exp_id: a.exp_id,
                rest: vec![a.path],
            })
            .await
        }
        Command::Skill(args) => commands::skill::run(args).await,
    }
}
