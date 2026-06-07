export const meta = {
  name: 'port-cli-to-rust',
  description: 'Port the openresearch-cli TypeScript CLI to Rust (tokio+reqwest+clap+serde) under rust/',
  phases: [
    { title: 'Foundation', detail: 'Scaffold crate, port shared infra (config, client, DTOs, dispatch), define contract' },
    { title: 'Commands', detail: 'One agent per command -> src/commands/<name>.rs against the contract' },
    { title: 'Integration', detail: 'cargo build and fix compile errors until the crate builds' },
    { title: 'Verify', detail: 'Adversarial fidelity check vs TS + build/clippy verification' },
  ],
}

const repoRoot = '/Users/rehaan/projects/openresearch-cli'
const tsDir = repoRoot + '/src'
const rustDir = repoRoot + '/rust'
const commands = [
  { name: 'artifact', ts: 'commands/artifact.ts' },
  { name: 'cat', ts: 'commands/cat.ts' },
  { name: 'chart', ts: 'commands/chart.ts' },
  { name: 'create_experiment', ts: 'commands/create-experiment.ts', verbs: ['create-experiment'] },
  { name: 'dev', ts: 'commands/dev.ts', verbs: ['dev'] },
  { name: 'experiments', ts: 'commands/experiments.ts' },
  { name: 'fs', ts: 'commands/fs.ts', verbs: ['read', 'write', 'str-replace', 'ls', 'grep', 'rm'] },
  { name: 'login', ts: 'commands/login.ts' },
  { name: 'logout', ts: 'commands/logout.ts' },
  { name: 'logs', ts: 'commands/logs.ts' },
  { name: 'projects', ts: 'commands/projects.ts' },
  { name: 'query', ts: 'commands/query.ts' },
  { name: 'runs', ts: 'commands/runs.ts' },
  { name: 'search', ts: 'commands/search.ts' },
  { name: 'search_logs', ts: 'commands/search-logs.ts', verbs: ['search-logs'] },
  { name: 'skill', ts: 'commands/skill.ts' },
  { name: 'tree', ts: 'commands/tree.ts' },
]

const CONTRACT_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['moduleLayout', 'commandEntrySignature', 'sharedApi', 'errorHandling', 'notesForCommandAuthors'],
  properties: {
    moduleLayout: { type: 'string', description: 'Tree of files created and what each holds' },
    commandEntrySignature: { type: 'string', description: 'Exact Rust signature each command module must expose, and how main.rs dispatches to it (clap subcommand -> module fn). Include how args/flags are passed in.' },
    sharedApi: { type: 'string', description: 'Signatures of every client.rs endpoint fn and every config/output helper a command author may call, with the DTO struct names. The API surface command authors build against.' },
    errorHandling: { type: 'string', description: 'The error type and the idiom for propagating + the requireCredentials equivalent.' },
    notesForCommandAuthors: { type: 'string', description: 'Anything non-obvious: stdin reading, PNG download, polling, browser open, exit codes, output formatting helpers.' },
  },
}

const BUILD_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['builds', 'summary', 'remainingIssues'],
  properties: {
    builds: { type: 'boolean', description: 'true if cargo build succeeds with no errors' },
    summary: { type: 'string', description: 'What was changed to make it build, and final cargo build/clippy status' },
    remainingIssues: { type: 'array', items: { type: 'string' }, description: 'Any warnings, TODOs, or unresolved fidelity gaps' },
  },
}

const VERIFY_SCHEMA = {
  type: 'object',
  additionalProperties: false,
  required: ['lens', 'verdict', 'findings'],
  properties: {
    lens: { type: 'string' },
    verdict: { type: 'string', enum: ['faithful', 'minor-gaps', 'major-gaps'] },
    findings: {
      type: 'array',
      items: {
        type: 'object',
        additionalProperties: false,
        required: ['severity', 'file', 'issue'],
        properties: {
          severity: { type: 'string', enum: ['blocker', 'major', 'minor'] },
          file: { type: 'string' },
          issue: { type: 'string' },
        },
      },
    },
  },
}

const moduleNames = commands.map((c) => c.name).join(', ')

// Phase 1: Foundation (sequential, alone -- owns all shared files)
phase('Foundation')
const foundationPrompt = [
  'You are porting a TypeScript CLI to Rust. This is the FOUNDATION step -- you own all shared infrastructure and the dispatch skeleton. Subsequent agents will each port ONE command into src/commands/<name>.rs against the contract you define, so be precise.',
  '',
  'SOURCE (read these first, in full):',
  '- ' + tsDir + '/index.ts        -- the entry point + command tree + every flag (the spec for the CLI surface)',
  '- ' + tsDir + '/client.ts       -- the HTTP client: ~20 typed endpoint fns + all response DTOs (port ALL of these)',
  '- ' + tsDir + '/config.ts       -- credential storage, XDG paths, requireCredentials, DEFAULT_API_URL / OPENRESEARCH_API_URL',
  '- ' + tsDir + '/table.ts        -- printTable + cell() output helpers',
  '- ' + tsDir + '/browser.ts      -- cross-platform open-in-browser',
  'Also skim a few command files (' + tsDir + '/commands/projects.ts, login.ts, chart.ts) to see how commands consume the shared API.',
  '',
  'TARGET: a new Rust crate at ' + rustDir + '/  (the dir does not exist yet -- create it).',
  'STACK (required): tokio (async, tokio::main), reqwest (json feature, async), clap (derive), serde/serde_json, plus a dirs/directories crate for XDG paths if helpful. The CLI must stay async-faithful to the fetch-based original.',
  '',
  'DELIVERABLES -- write these files:',
  '1. ' + rustDir + '/Cargo.toml  -- package "openresearch-cli", binary "orx", all deps with right features (reqwest json, clap derive, tokio rt-multi-thread+macros, serde derive). No workspace.',
  '2. ' + rustDir + '/src/error.rs -- a single crate error type (anyhow OR thiserror; commands propagate with the ? operator). Include the requireCredentials equivalent that loads creds or exits(1) with "Not logged in. Run orx login first.".',
  '3. ' + rustDir + '/src/config.rs -- Credentials struct, XDG config path (~/.config/openresearch/credentials.json honoring XDG_CONFIG_HOME), load/save (unix mode 0600)/clear, DEFAULT_API_URL honoring OPENRESEARCH_API_URL env (default http://localhost:4000).',
  '4. ' + rustDir + '/src/client.rs -- port EVERY endpoint fn and EVERY DTO from client.ts. Same paths, methods, query/body shapes. Port request() error semantics EXACTLY: 401 -> "Unauthorized -- your token is invalid or revoked. Run orx login again.", non-2xx -> surface server body text, network error -> "Could not reach the API at {url}: ...". Use serde with serde(rename_all = "camelCase") so JSON matches; preserve enums like syncStatus and the DevFsOp tagged union (serde tag = "op").',
  '5. ' + rustDir + '/src/output.rs (and/or browser.rs) -- printTable, cell(), openBrowser equivalents.',
  '6. ' + rustDir + '/src/main.rs -- a clap derive Parser with a Subcommand enum covering EVERY command and flag exactly as in index.ts USAGE (login, logout, projects --all, experiments, runs --experiment, logs --head/--bytes/--range, search-logs, search, tree, cat, artifact --head/--bytes, query, chart wandb..., create-experiment..., dev open|close|status -m/--discard, the fs verbs read/write/str-replace/ls/grep/rm, skill). tokio::main async main that dispatches each subcommand to its module fn. The fs verbs all route to commands::fs.',
  '7. STUB every command module so the tree compiles before commands are filled in: create ' + rustDir + '/src/commands/mod.rs declaring all modules, and ' + rustDir + '/src/commands/<name>.rs for each module name: ' + moduleNames + '. Each stub exposes the real entry-fn signature you choose with a todo!() body so cargo build links. Name mapping: kebab commands -> snake_case modules (create-experiment->create_experiment, search-logs->search_logs); all fs verbs live in the fs module.',
  '',
  'Pick the command entry-fn convention NOW and make every stub match it (e.g. pub async fn run(creds, parsed_args) -> anyhow::Result<()>). Decide how parsed clap args reach each command and document it precisely.',
  '',
  'After writing, run cargo build in ' + rustDir + ' if cargo is available (todo!() compiles). Fix any compile errors in the shared files. Report the contract precisely so command authors do not have to guess.',
].join('\n')

const contract = await agent(foundationPrompt, { label: 'foundation', schema: CONTRACT_SCHEMA })

// Phase 2: Port each command in parallel (each owns exactly one file)
phase('Commands')
const contractText = JSON.stringify(contract, null, 2)
const ported = await parallel(commands.map((c) => () => {
  const verbLine = c.verbs ? '  (implements the CLI verb(s): ' + c.verbs.join(', ') + ')' : ''
  const prompt = [
    'Port ONE command of the openresearch CLI from TypeScript to Rust.',
    '',
    'YOUR FILE (the ONLY file you may write): ' + rustDir + '/src/commands/' + c.name + '.rs',
    'SOURCE TO PORT: ' + tsDir + '/' + c.ts + verbLine,
    '',
    'Read the TS source IN FULL and reproduce its behavior exactly in Rust: same API calls, same flag/positional handling, same output formatting (table layout, prefixes like the check mark, numeric formatting, exit codes via std::process::exit or returning an error), same edge cases (missing args -> usage to stderr + exit 1, stdin reading, polling loops, file writes, browser opening, PNG download, etc.).',
    '',
    'Build STRICTLY against the foundation contract below -- do not invent new shared APIs, do not edit any shared file (config.rs, client.rs, output.rs, main.rs, mod.rs). If you need a client endpoint or helper, it already exists per the contract; call it. Match the EXACT command entry-fn signature the contract specifies -- main.rs already dispatches to it.',
    '',
    'FOUNDATION CONTRACT:',
    contractText,
    '',
    'If the TS file uses other shared modules (table/cell, browser, config, client), use the Rust equivalents named in the contract. Overwrite the stub at your file path with the complete implementation. Do NOT add mod declarations anywhere else. Keep idiomatic Rust. Return a one-paragraph summary of what you ported and any contract mismatch you hit (so integration can reconcile).',
  ].join('\n')
  return agent(prompt, { label: 'port:' + c.name, phase: 'Commands' })
}))

// Phase 3: Integration -- make the whole crate build (barrier: needs all commands)
phase('Integration')
const integrationPrompt = [
  'All command modules of the Rust port at ' + rustDir + ' have been written by separate agents against a shared contract. Make the entire crate BUILD and behave like the TS original.',
  '',
  '1. cd ' + rustDir + ' and run cargo build. If cargo is not installed, say so clearly in remainingIssues and instead do a careful manual review for obvious type/signature mismatches.',
  '2. Fix ALL compile errors. Common causes: a command author drifted from the contract (wrong entry-fn signature, wrong client fn name/shape), missing serde attrs, ownership/lifetime issues, an unhandled clap subcommand arm, a stub left as todo!(). You MAY edit any file now to reconcile -- prefer fixing the command to match the contract over changing the shared API, unless the shared API was wrong.',
  '3. Re-run until cargo build is clean. Then run cargo clippy if available and fix easy warnings (unused imports, needless clones); note hard ones.',
  '4. Sanity-check the surface: cargo run -- --help should list every command from ' + tsDir + '/index.ts. A no-arg run should print usage without panicking.',
  '',
  'Cross-reference ' + tsDir + '/index.ts and ' + tsDir + '/client.ts as the source of truth when reconciling. Report whether it builds, what you changed, and any remaining gaps.',
].join('\n')
const build = await agent(integrationPrompt, { label: 'integration', schema: BUILD_SCHEMA })

// Phase 4: Adversarial verification across lenses (parallel)
phase('Verify')
const LENSES = [
  { key: 'api-fidelity', prompt: 'Compare ' + rustDir + '/src/client.rs against ' + tsDir + '/client.ts. Verify EVERY endpoint was ported with the exact same path, HTTP method, query params, request body shape, and response DTO fields (camelCase rename, enums, the DevFsOp tagged union). Flag any missing endpoint, wrong path, wrong method, dropped field, or mismatched error-message semantics (401 text, network-error text, server-body surfacing). Be adversarial -- assume something was dropped and prove otherwise.' },
  { key: 'cli-surface', prompt: 'Compare the clap definition in ' + rustDir + '/src/main.rs against the USAGE + parseArgs options in ' + tsDir + '/index.ts. Verify every command, subcommand, positional, and flag (--all, --experiment, --head, --bytes, --range, --run repeatable, --max, --metric, --smoothing, --out, --title, --description, --parent, --repo, --ref, -m/--message, --discard, -h) exists with the right arity/type and dispatches to the right module. Flag any missing or misnamed command/flag, wrong multiplicity (--run must be repeatable), or wrong default.' },
  { key: 'command-behavior', prompt: 'Spot-check the trickiest command ports for behavioral fidelity vs their TS sources: login (loopback HTTP server + nonce/state + 5min timeout + success HTML), chart (server render -> download PNG -> write to XDG cache dir -> print path), dev (provision + 3s poll loop + 5min timeout + status/close commit messages), fs (stdin read for write, str-replace arity, verb->DevFsOp mapping), logs/artifact (head/tail/range byte params). Read ' + rustDir + '/src/commands/{login,chart,dev,fs,logs,artifact}.rs vs ' + tsDir + '/commands/. Flag missing edge cases, wrong exit codes, or wrong output text.' },
]
const reviews = await parallel(LENSES.map((l) => () =>
  agent(l.prompt + '\n\nReturn a verdict and concrete findings. Only report real, verifiable discrepancies (cite file + what differs from the TS). Do not nitpick idiomatic-Rust style that preserves behavior.',
    { label: 'verify:' + l.key, phase: 'Verify', schema: VERIFY_SCHEMA })
    .then((v) => Object.assign({}, v, { lens: l.key }))))

return {
  contract,
  commandsPorted: ported.filter(Boolean).length,
  build,
  review: reviews.filter(Boolean),
}
