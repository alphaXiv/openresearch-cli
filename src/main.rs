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
mod jobs;
// Local mode (`orx up`): builds out across stages; not all of it is wired yet.
#[allow(dead_code)]
mod local;
mod output;
mod remote;
mod store;
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

    /// Browse the public project directory (no membership needed).
    Explore(ExploreArgs),

    /// Operate on one project (view it, or edit its name / description).
    Project(ProjectArgs),

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

    /// Spin up standalone compute in an organization (no experiment).
    Instance(InstanceArgs),

    /// Operate on one experiment node (status / run command / run / cancel).
    Exp(ExpArgs),

    /// Upload, list, show, or download a project's research reports.
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

    /// Loopback HTTP/SSE daemon over the local run store (jobs sibling of
    /// `opencode serve`); the api tunnels to it on agent boxes.
    Serve(ServeArgs),

    /// Supervise one external run: tail backend logs, mirror status to the
    /// api, honor cancel intent. Spawned detached by `exp run --backend hf`;
    /// safe to re-run after a crash or box replacement.
    Supervise(SuperviseArgs),

    /// Start the local autoresearch dashboard on 127.0.0.1: embedded UI,
    /// JSON/SSE API over the local store, and the opencode agent proxy.
    Up(UpArgs),
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
    /// Emit raw JSON (id, name, paperId, repo, org) instead of the formatted
    /// table — for scripts that need each project's `paperId`.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ExploreArgs {
    /// Emit raw JSON instead of the formatted table.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct ProjectArgs {
    #[command(subcommand)]
    pub command: ProjectCommand,
}

#[derive(Subcommand, Debug)]
pub enum ProjectCommand {
    /// Show a project's overview: details, experiment tree, and reports.
    View { project_id: String },

    /// Edit a project's metadata. Pass at least one of `--name` / `--description`
    /// / `--public` / `--private` / `--run-command`.
    Edit {
        project_id: String,
        /// Rename the project.
        #[arg(long)]
        name: Option<String>,
        /// Set the project's default run command (local projects only).
        /// New experiments inherit it; pass '' to clear.
        #[arg(long = "run-command")]
        run_command: Option<String>,
        /// Overwrite the project's description with this value.
        #[arg(long)]
        description: Option<String>,
        /// Overwrite the description with the whole of stdin (for long markdown).
        #[arg(long)]
        description_stdin: bool,
        /// Make the project public (listed in the public directory).
        #[arg(long)]
        public: bool,
        /// Make the project private. Mutually exclusive with `--public`.
        #[arg(long, conflicts_with = "public")]
        private: bool,
    },
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
    /// Branch of the repo the project binds to (with `--repo`; defaults to the
    /// repo's default branch). The baseline experiment branches off it.
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
    /// Parent experiment id -> create a child. Omit on an empty project to
    /// create the baseline (root); once a root exists, local projects attach
    /// under the oldest root (server projects create another baseline).
    #[arg(long)]
    pub parent: Option<String>,
    /// Create a new baseline (root) even when the project already has one.
    /// Conflicts with --parent. Projects may hold multiple baselines.
    #[arg(long, conflicts_with = "parent")]
    pub baseline: bool,
    /// Run command for the node (local projects and server baselines). Omit to
    /// inherit from the parent / project default.
    #[arg(long = "run-command")]
    pub run_command: Option<String>,
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
    /// Filter to one provider (e.g. `runpod`, `vast`, `lambda`). Case-insensitive. GPU mode only.
    #[arg(long)]
    pub provider: Option<String>,
}

#[derive(Args, Debug)]
pub struct InstanceArgs {
    #[command(subcommand)]
    pub command: InstanceCommand,
}

#[derive(Subcommand, Debug)]
pub enum InstanceCommand {
    /// Provision a standalone instance in an org (GPU with `--gpu`, or CPU with
    /// `--cpu`). Not tied to an experiment — like the dashboard's "Spin up".
    Create(InstanceCreateArgs),
    /// List an org's instances (status, SSH endpoint, price) — including any
    /// `--backend openresearch` box a failed teardown left behind.
    List(InstanceListArgs),
    /// Terminate an instance (destroys the provider machine). The manual
    /// cleanup path when a run's automatic teardown failed.
    Delete(InstanceDeleteArgs),
}

#[derive(Args, Debug)]
pub struct InstanceCreateArgs {
    /// Organization id (from `orx projects`).
    pub org_id: String,
    /// Provision a GPU instance with this GPU id, e.g. `H100_SXM` — the exact id
    /// from `orx compute`, not a family name like `H100`.
    #[arg(long)]
    pub gpu: Option<String>,
    /// GPUs per instance (with `--gpu`; default 1).
    #[arg(long)]
    pub count: Option<i64>,
    /// Disk in GB (with `--gpu`; default 100).
    #[arg(long)]
    pub disk: Option<i64>,
    /// Provider to provision from (with `--gpu`), e.g. runpod, vast, lambda.
    /// Omit to pick the cheapest matching offer across providers (like the
    /// dashboard). See `orx compute` for providers; validated server-side.
    #[arg(long)]
    pub provider: Option<String>,
    /// Provision a CPU-only instance with this flavor: cpu5c (compute), cpu5g
    /// (general), or cpu5m (memory-optimized). Mutually exclusive with `--gpu`.
    #[arg(long)]
    pub cpu: Option<String>,
    /// vCPUs for a CPU instance (with `--cpu`): 2, 8, or 32 (default 8).
    #[arg(long)]
    pub vcpus: Option<i64>,
}

#[derive(Args, Debug)]
pub struct InstanceListArgs {
    /// Organization id (from `orx projects`).
    pub org_id: String,
}

#[derive(Args, Debug)]
pub struct InstanceDeleteArgs {
    /// The instance (sandbox) id to terminate.
    pub sandbox_id: String,
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
    Run(Box<ExpRunArgs>),

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

    /// Print a report's markdown body to stdout. Pass its id or slug.
    Show {
        project_id: String,
        /// Report id (from `orx report list`) or its slug.
        report: String,
    },

    /// Download a report folder (report.md + referenced images) to a local
    /// directory — the inverse of `upload`. Pass the report's id or slug.
    Download {
        project_id: String,
        /// Report id (from `orx report list`) or its slug.
        report: String,
        /// Destination directory (created if absent). `report.md` and an
        /// `images/` subfolder are written under it.
        dir: String,
    },
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
    /// Disk in GB (with `--gpu` or a `--backend openresearch` GPU flavor;
    /// default 100).
    #[arg(long)]
    pub disk: Option<i64>,
    /// Provider to provision from (with `--gpu` or a `--backend openresearch`
    /// GPU flavor), e.g. runpod, vast, lambda. Defaults to runpod (`--gpu`) or
    /// the cheapest offer (`openresearch`) when omitted; validated server-side.
    #[arg(long)]
    pub provider: Option<String>,
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
    /// External executor instead of managed compute: `hf` (Hugging Face Jobs,
    /// billed to your HF account), `modal` (a Modal Sandbox on your own Modal
    /// account, billed per second), `k8s` (a Job on your own Kubernetes
    /// cluster), `ssh` (a detached process on one of your own boxes), `slurm`
    /// (a batch job on your Slurm cluster, submitted via its login node),
    /// `openresearch` (an ephemeral OpenResearch GPU/CPU box billed to your
    /// org; needs `orx login`), or `local` (a detached process on this
    /// machine). k8s, ssh, slurm, openresearch, and local are local
    /// experiments only. orx submits the job and a detached supervisor
    /// mirrors status/logs back.
    #[arg(long)]
    pub backend: Option<String>,
    /// Hardware flavor. With `--backend hf`: t4-small, a10g-small, a100-large,
    /// h200, … With `--backend modal`: a Modal GPU (t4, l4, a10g, a100,
    /// a100-80gb, l40s, h100, h200, or e.g. h100:2) or cpu/cpu-large. With
    /// `--backend slurm`: a GPU request as a GRES spec (h100:2 → --gres=gpu:h100:2;
    /// plain `gpu` → one GPU; omit for CPU-only). With `--backend openresearch`:
    /// a GPU id from `orx compute` (h100_sxm, or h100_sxm:2 for two) or a CPU
    /// flavor (cpu5c/cpu5g/cpu5m, or cpu5c:32 for the vCPU tier). Not used by
    /// k8s (see --manifest) or ssh (see --host).
    #[arg(long)]
    pub flavor: Option<String>,
    /// The org to bill the box to (with `--backend openresearch`). Omit when
    /// you belong to exactly one org.
    #[arg(long)]
    pub org: Option<String>,
    /// The ~/.ssh/config host alias to run on (with `--backend ssh`), or the
    /// cluster login node (with `--backend slurm`; defaults to the slurm
    /// settings' host).
    #[arg(long)]
    pub host: Option<String>,
    /// Repo-relative path to the k8s manifest on the experiment branch (with
    /// `--backend k8s`; default .orx/k8s.yaml). The manifest declares the run's
    /// resources — image, GPUs, topology — and orx injects the run script, env
    /// Secret, labels, and a default timeout. See `orx skill` for the contract.
    #[arg(long)]
    pub manifest: Option<String>,
    /// Docker image for the job (with `--backend hf/modal`). Defaults to
    /// python:3.12 on CPU flavors, a CUDA pytorch image otherwise. With
    /// `--backend k8s`, set the image in the manifest instead.
    #[arg(long)]
    pub image: Option<String>,
    /// Job timeout (with `--backend hf/modal/k8s/slurm/openresearch`): 90s,
    /// 30m, 4h, 1d. Default 4h (HF's own default is only 30 minutes). With
    /// `--backend k8s` it becomes activeDeadlineSeconds unless the manifest
    /// sets its own. With `--backend slurm` it becomes `#SBATCH --time=` and
    /// has no 4h default — unset falls back to the slurm settings, then the
    /// cluster's own limit. With `--backend openresearch` it bounds the run's
    /// wall clock on the box (the box itself is deleted when the run ends).
    #[arg(long)]
    pub timeout: Option<String>,
    /// Launch even if the experiment's branch has no changes over its parent
    /// (bypasses the "did you forget to push?" guard, for a deliberate re-run).
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct ServeArgs {
    /// Port to bind on 127.0.0.1 (default 4790 — what the api proxies to).
    #[arg(long)]
    pub port: Option<u16>,
}

#[derive(Args, Debug)]
pub struct SuperviseArgs {
    /// The run to supervise (must exist in the local store).
    pub run_id: String,
}

#[derive(Args, Debug)]
pub struct UpArgs {
    /// Port to bind on 127.0.0.1. With `--remote`, the local port to forward.
    #[arg(long, default_value_t = 4791)]
    pub port: u16,
    /// Run `orx up` on a remote box over SSH and forward it here. The value is
    /// an `~/.ssh/config` host alias (or `user@host`). Starts the server there,
    /// tunnels `--port` to your laptop, and opens your browser. Note: the remote
    /// dashboard is unauthenticated and bound to that host's loopback, so anyone
    /// else with an account on that host can reach it.
    #[arg(long, value_name = "HOST")]
    pub remote: Option<String>,
    /// Don't open the dashboard in the browser on startup.
    #[arg(long)]
    pub no_browser: bool,
    /// Don't spawn the opencode agent on startup (for tests).
    #[arg(long)]
    pub no_agent: bool,
    /// opencode model override, e.g. `anthropic/claude-sonnet-4-5`.
    #[arg(long)]
    pub model: Option<String>,
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
    // Outdated-version warning (skipped for the commands that manage updates
    // themselves). `start` prints the cached warning to stderr *now*,
    // before the command runs, so it shows even for commands that
    // `std::process::exit` on their own (e.g. the "not logged in" path) instead
    // of returning here. Never touches stdout or the exit code. Silence it with
    // ORX_NO_UPDATE_CHECK / NO_UPDATE_NOTIFIER.
    let warning = (!matches!(command, Command::Version(_) | Command::Update(_)))
        .then(updates::UpdateWarning::start);

    let result = dispatch(command).await;
    if let Some(warning) = warning {
        warning.finish().await;
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
        Command::Explore(args) => commands::explore::run(args).await,
        Command::Project(args) => commands::project::run(args).await,
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
        Command::Instance(args) => commands::instance::run(args).await,
        Command::Exp(args) => commands::exp::run(args).await,
        Command::Report(args) => commands::report::run(args).await,
        Command::Skill(args) => commands::skill::run(args).await,
        Command::InstallSkills(args) => commands::install_skills::run(args).await,
        Command::Lit(args) => commands::lit::run(args).await,
        Command::Paper(args) => commands::paper::run(args).await,
        Command::Version(args) => commands::version::run(args).await,
        Command::Update(args) => commands::update::run(args).await,
        Command::Serve(args) => commands::serve::run(args).await,
        Command::Supervise(args) => commands::supervise::run(args).await,
        Command::Up(args) => match args.remote.clone() {
            Some(host) => commands::up_remote::run(&host, args).await,
            None => commands::up::run(args).await,
        },
    }
}
