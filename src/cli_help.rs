pub const CLI: &str = r#"Get started:
  Run these commands inside the repository you want to search:

    sonar init
    sonar investigate "Where are incoming requests authenticated?"

Examples:
  Get a short list of places to start reading:
    sonar brief "How are failed requests retried?" --limit 3

  Discover declarations and their navigation handles:
    sonar outline src/main.rs

  Find the declaration at a source line:
    sonar locate src/main.rs --line 10

  Narrow a search and skip an irrelevant directory:
    sonar investigate "How are requests routed?" --scope src --exclude src/generated

  Refresh using saved project settings:
    sonar init

  Search another repository's existing index:
    sonar --db /path/to/repo/.sonar/index.sqlite brief "How do I run tests?"

Read the results:
  Commands return JSON with file paths, symbols, and source line ranges.
  Open the returned files with your editor or your agent's file tools.
  Copy full handle values for follow-up navigation; IDs such as s1 are not handles.
  Check index and notices for source freshness and missing preparation.

Use with AI agents:
  Initialize and connect a client in one command:
    sonar setup codex
    sonar setup claude

  Setup writes the client's MCP configuration and adds usage instructions to
  AGENTS.md or CLAUDE.md. Reload the client, then ask it to call sonar_status.
  An agent with terminal access can also run Sonar commands directly.
  AI setup guide: https://github.com/BeepBoopBit/sonar-code

Storage and optional preparation:
  Basic search runs locally without a model or API key.
  The default index is .sonar/index.sqlite in your repository. Init adds its
  ignore rule and saves settings in .sonar/project.json. Project commands reuse
  the saved database path. Each index belongs to one repository at its absolute path.
  Init includes document/resource links and module navigation. Use init --resolve
  or init --typescript PATH for optional Rust or TypeScript calls and references.
  Use init --model PATH to save optional embedding preparation and hybrid search.
  Queries do not download models. The optional source-checkout downloader stores
  shared models in ~/.sonar/models/; see "sonar embed --help".
  After edits, rerun init to refresh saved preparation; obtain fresh handles.

Run "sonar <command> --help" for command options and examples."#;

pub const INDEX: &str = r#"For first-time setup or a saved project refresh, prefer "sonar init". This
lower-level command indexes files without running the full saved preparation.

Examples:
  Index the current repository, or refresh it after source changes:
    sonar index .

  Store a repository's index at a custom path:
    sonar --db /path/to/repo/.sonar/index.sqlite index /path/to/repo

  Also index bounded YAML resources:
    sonar index . --resources yaml

Supports Rust, Python, TypeScript/TSX, and Markdown. Indexing respects repository
.gitignore and .ignore rules, skips hidden files and build/dependency directories,
and leaves source files unchanged. Basic search needs no model or API key.

Next, run:
  sonar investigate "Where are incoming requests authenticated?"

Keep the same --db when querying a custom index. After edits, repeat the original
indexing options and any navigation, resolution, or embedding preparation steps.
There is no automatic file watcher. Obtain fresh handles after reindexing."#;

pub const INVESTIGATE: &str = r#"Examples:
  Find entry points for a code question:
    sonar investigate "Where are incoming requests authenticated?"

  Narrow the starting locations and exclude an irrelevant area:
    sonar investigate "How are requests routed?" --scope src --exclude src/generated

  Expand a chosen lead instead of searching the whole repository again:
    sonar investigate "How are requests routed?" --from 'RETURNED_HANDLE'

  Use optional embeddings after preparing them with the same local model:
    sonar investigate "request authentication" --model /path/to/model

First run "sonar init" inside your repository. Basic discovery needs no model.
A model saved by init is reused automatically for hybrid discovery.
The JSON panels group leads; locations provide paths, handles, and suggested focus
line ranges. Read those ranges with your editor or your agent's file tools.

Replace RETURNED_HANDLE with a full handle from investigate, outline, locate,
or brief --handles. Obtain fresh handles after reindexing. Recorded calls and
references require optional compiler resolution; init includes basic navigation.
Read index and notices for freshness, preparation state, and language coverage."#;

pub const BRIEF: &str = r#"Examples:
  Get a short list of places to start reading:
    sonar brief "How are failed requests retried?" --limit 3

  Include full handles for follow-up navigation:
    sonar brief "request authentication" --handles

  Search implementation functions in a directory:
    sonar brief "request routing" --role implementation --kind function --scope src

  Exclude multiple areas:
    sonar brief "configuration" --exclude src/generated --exclude tests

  Use optional embeddings after running embed with the same local model:
    sonar brief "request authentication" --retrieval hybrid --model /path/to/model

First run "sonar init" inside your repository. Basic search needs no model.
With a model saved by init, retrieval defaults to hybrid; otherwise lexical.
Use --retrieval lexical to select model-free search explicitly.
Output is JSON: starts lists ranked location IDs, and files contains their paths,
symbols, and line ranges. Read the source with your editor or agent file tools.
Use a location's full handle for navigation, rather than a short ID such as s1.
Check index and notices for freshness and missing optional preparation."#;

pub const OUTLINE: &str = r#"Examples:
  List declarations in an indexed file:
    sonar outline src/main.rs

  Return a smaller page:
    sonar outline src/main.rs --limit 10

  Continue using the cursor from the previous response:
    sonar outline src/main.rs --limit 10 --cursor 'RETURNED_CURSOR'

Replace src/main.rs with a path returned by your search. Paths are relative to
the indexed repository. The JSON locations include full handles and parent
relationships; pass a handle to neighbors or investigate --from.
Keep the same file and page limit when continuing an outline.
An existing index is required; no model or compiler resolution is needed."#;

pub const LOCATE: &str = r#"Examples:
  Find the smallest indexed declaration covering a source line:
    sonar locate src/main.rs --line 10

  Specify an exact position using a UTF-8 byte column:
    sonar locate src/main.rs --line 10 --column 5

Use a repository-relative path and positive, one-based line and column numbers.
Copy the returned full handle to neighbors, follow, or investigate --from.
An existing index is required; no model or compiler resolution is needed."#;

pub const NEIGHBORS: &str = r#"Examples:
  Inspect relationships around a known location:
    sonar neighbors 'RETURNED_HANDLE'

  Find callers after resolving compiler relationships:
    sonar neighbors 'RETURNED_HANDLE' --relation calls --direction incoming

  Restrict results to a directory:
    sonar neighbors 'RETURNED_HANDLE' --scope src --limit 10

  Continue the same query with its returned cursor:
    sonar neighbors 'RETURNED_HANDLE' --scope src --limit 10 --cursor 'RETURNED_CURSOR'

Copy a full handle from locate, outline, investigate, or brief --handles.
Keep the same handle and options when continuing a page. Get new handles after
reindexing. Calls and references need resolve for the relevant language;
document/resource links and module navigation need index-navigation.
Output includes recorded relationships and their source evidence."#;

pub const FOLLOW: &str = r#"Examples:
  Search recorded connections around a selected location:
    sonar follow 'RETURNED_HANDLE' "request authentication"

  Search callers within two hops:
    sonar follow 'RETURNED_HANDLE' "retry" --relation calls --direction incoming --hops 2

  Restrict returned matches to functions in a directory:
    sonar follow 'RETURNED_HANDLE' "routing" --kind function --scope src --limit 3

First prepare relationships with "sonar init --resolve" for Rust, or
"sonar init --typescript PATH" for TypeScript. Copy a full handle from
locate, outline, investigate, or brief --handles. Reindexing requires new handles.
The JSON matches include an evidenced trail through recorded calls/references
and suggested source ranges. Read those ranges to verify the behavior.
Basic lexical matching needs no embedding model."#;

pub const FIELD_USES: &str = r#"Examples:
  Inspect uses around a known field or function:
    sonar field-uses 'RETURNED_HANDLE'

  Restrict use sites to a directory:
    sonar field-uses 'RETURNED_HANDLE' --scope src --limit 10

  Continue using the returned cursor:
    sonar field-uses 'RETURNED_HANDLE' --scope src --limit 10 --cursor 'RETURNED_CURSOR'

First use "sonar init --resolve" for Rust or "sonar init --typescript PATH"
for TypeScript relationships. Obtain a full field/function handle from outline or locate.
The scope restricts use-site files; declarations can be outside that directory.
Keep the same handle and options across pages, and get fresh handles after
reindexing. Python compiler-resolved field uses are unavailable."#;

pub const MCP: &str = r#"Setup:
  Initialize and register the current repository with your AI client:
    sonar setup codex
    sonar setup claude

  For manual registration, configure an absolute executable path and arguments
  equivalent to this command; --root also allows startup before initialization:
    sonar --db /absolute/path/to/repo/.sonar/index.sqlite mcp --root /absolute/path/to/repo

  Your client starts Sonar and communicates through stdin/stdout. Running the
  server directly in a terminal waits for client input. No port is needed.
  The client must also be able to read the indexed source tree.

Verify and use:
  Reload your client or start a new session. Call sonar_status with {} and check
  the project path. If initialized is false, call sonar_init with {} to prepare
  this bound repository. Use sonar_investigate or sonar_brief to find code, then
  read the returned source ranges with the agent's file tools.

Only sonar_init writes project state. Query/navigation tools remain read-only
and require an initialized index. The root is fixed by server configuration;
sonar_init cannot redirect it or register an AI client. Refresh saved preparation
with sonar init or sonar_init, then obtain fresh handles.
Basic search needs no model or API key. A model saved by init is reused for hybrid
search; --model PATH overrides it. Assets must already exist; no automatic downloads.

AI setup guide: https://github.com/BeepBoopBit/sonar-code"#;

pub const RESOLVE: &str = r#"Examples:
  Resolve Rust calls and references with rust-analyzer:
    sonar resolve

  Use a specific rust-analyzer executable and JSON configuration:
    sonar resolve --analyzer /path/to/rust-analyzer --config /path/to/config.json

  Resolve TypeScript using a local compiler module:
    sonar resolve --typescript /path/to/node_modules/typescript

  Select a TypeScript project configuration:
    sonar resolve --typescript /path/to/node_modules/typescript --project /path/to/tsconfig.json

First run "sonar index .". Rust resolution needs rust-analyzer, the repository's
Rust toolchain, and locally available dependencies; Cargo runs locked and offline.
TypeScript resolution needs Node.js, a local TypeScript compiler, and project
dependencies. Run both resolution commands for a mixed Rust/TypeScript repository.
The --config option supplies Rust analyzer settings; --project selects TypeScript.

Next, obtain a handle with outline or locate, then use neighbors or follow.
Repeat resolution after relevant source, dependency, or configuration changes.
Basic search works without this optional preparation."#;

pub const INDEX_NAVIGATION: &str = r#"Examples:
  Prepare document/resource links and module navigation after indexing:
    sonar index .
    sonar index-navigation

  Include YAML resources before preparing navigation:
    sonar index . --resources yaml
    sonar index-navigation

  Prepare navigation for a custom index:
    sonar --db /path/to/repo/.sonar/index.sqlite index-navigation

Init already runs this step. This lower-level command records source-backed
links and supported module relationships after a manual index refresh.
Calls and references need resolve instead. No embedding model is required.
After edits, reindex with the original options and repeat this command.
Inspect links with neighbors, investigate --from, or supported navigation options."#;

pub const EMBED: &str = r#"Examples:
  Build search vectors from a model already stored locally:
    sonar embed --model /path/to/model

  Search using those vectors and the same model:
    sonar investigate "request authentication" --model /path/to/model
    sonar brief "request authentication" --retrieval hybrid --model /path/to/model

Optional model download (requires Python 3 and a Sonar source checkout):
  python3 /path/to/sonar/scripts/download_model.py --model potion
  sonar embed --model "$HOME/.sonar/models/potion-code-16M-v2"

First run "sonar init". To save the model and refresh embeddings automatically,
use "sonar init --model /path/to/model" instead of a standalone embed command.
Project searches then reuse that model and default to hybrid retrieval.
Basic search works without embeddings or an API key.
The downloader fetches pinned, checksum-verified assets; Cargo installs the Sonar
executable, while this helper script is available in the source repository.
Downloads default to ~/.sonar/models/ and are shared across repositories. Valid
files are reused; use --destination /path/to/model for a custom download path.
Model files are used locally for search; your AI keeps using its existing model.
Queries do not download assets. Use the same model directory for embedding,
queries, and MCP. Refresh with init for saved preparation, or repeat embed after
a manual reindex. Installing a model with the downloader does not configure Sonar.

Model setup guide: https://github.com/BeepBoopBit/sonar-code"#;

pub const EMBED_USAGE: &str = r#"Examples:
  Prepare optional caller vectors for a Rust repository:
    sonar index .
    sonar resolve
    sonar embed --model /path/to/model
    sonar embed-usage --model /path/to/model

  Include caller evidence in hybrid search:
    sonar brief "request retries" --retrieval hybrid --model /path/to/model --usage

Requires current Rust compiler relationships and source embeddings. Use the same
local model throughout. After source changes, repeat indexing, resolution, embed,
and embed-usage before querying. Init can save resolution and source embedding
steps, but embed-usage must still be run separately after each refresh.
Basic search does not need this preparation."#;

pub const INDEX_QUANTITIES: &str = r#"Examples:
  Prepare exact integer quantity evidence from indexed Rust sources:
    sonar index .
    sonar index-quantities

  Include byte/time quantity evidence in a search:
    sonar brief "timeout of 30 seconds" --quantities

This optional step reads current Rust sources. It is separate from the saved
init preparation; rebuild it after reindexing, including after init.
Basic search needs neither quantity preparation nor an embedding model."#;

pub const IMPORT_SCIP: &str = r#"Examples:
  Import a compiler export with its matching source-bound Sonar manifest:
    sonar import-scip /path/to/index.scip --manifest /path/to/semantic-manifest.json

  Import into a particular repository's existing index:
    sonar --db /path/to/repo/.sonar/index.sqlite import-scip /path/to/index.scip --manifest /path/to/semantic-manifest.json

First index the same repository snapshot. The export and manifest must match
the indexed root, source snapshot, analysis inputs, and export checksum.
For ordinary compiler preparation, use "sonar resolve" instead."#;

pub const DIAGNOSE: &str = r#"Examples:
  Inspect retrieval candidates and their ranking scores:
    sonar diagnose "request authentication"

  Investigate a search with the same filters as brief:
    sonar diagnose "request routing" --scope src --exclude src/generated

  Inspect optional hybrid retrieval:
    sonar diagnose "request authentication" --retrieval hybrid --model /path/to/model

Prepare the index first, and embeddings when using embedding or hybrid retrieval.
Output is JSON with candidate ranks and retrieval-channel scores.
Use the same options as the brief you are investigating."#;

pub const BATCH: &str = r#"Examples:
  Search a file containing one JSON string per line:
    sonar batch < queries.jsonl

  Return diagnostics for each query:
    sonar batch --diagnostics < queries.jsonl

  Reuse a prepared local model across hybrid searches:
    sonar batch --retrieval hybrid --model /path/to/model < queries.jsonl

Example queries.jsonl contents:
  "Where are incoming requests authenticated?"
  "How are failed requests retried?"

Prepare the index first, and embeddings when using embedding or hybrid retrieval.
Each input line must be a JSON string. Output is one JSON response per line.
Basic lexical search needs no model; an optional supplied model stays loaded."#;

pub const STRUCTURE: &str = r#"Examples:
  Export indexed structural features as JSON:
    sonar structure

  Restrict observations to a repository-relative file or directory:
    sonar structure --scope src

An existing index is required. This read-only export supports offline ranking
research. Use investigate or brief for ordinary code navigation."#;

pub const INIT: &str = r#"Examples:
  Prepare the current repository without a model:
    sonar init

  Initialize another repository:
    sonar init /path/to/repo

  Remember optional YAML resources:
    sonar init --resources yaml

  Also prepare Rust calls and references:
    sonar init --resolve

  Also prepare TypeScript relationships using a local compiler:
    sonar init --typescript /path/to/node_modules/typescript

  Also prepare embedding search with an existing local model:
    sonar init --model /path/to/model

Initialization indexes supported files, prepares basic navigation, adds .sonar/
and any custom in-project database to .gitignore, and saves settings in
.sonar/project.json. Repeating it refreshes saved resources, chunking, resolution,
and source embedding preparation. Basic setup needs no model, API key, or compiler
resolution. Existing valid indexes are adopted, including those without settings.
Commands from this project or its subdirectories reuse saved paths and settings.
With a saved model, investigate, brief, batch, diagnose, and MCP use hybrid search
by default. Other optional steps, such as embed-usage, are run separately.

Optional --resolve needs rust-analyzer and local project dependencies. TypeScript
analysis needs Node.js, a local compiler module, and project dependencies.
Model assets must already exist; Sonar does not download them automatically.
Next, search with sonar investigate or connect a client with sonar setup codex/claude.
MCP clients can perform the same initialization using the sonar_init tool."#;

pub const SETUP: &str = r#"Examples:
  Initialize this repository and configure Codex:
    sonar setup codex

  Configure Claude Code:
    sonar setup claude

  Configure another repository and prepare Rust relationships:
    sonar setup codex /path/to/repo --resolve

  Register a distinct connection for your user across projects:
    sonar setup claude --global

  Choose a distinct server name:
    sonar setup claude --name sonar-code

  Replace a conflicting registration explicitly:
    sonar setup codex --replace

Setup initializes the repository, verifies the local MCP server with sonar_status,
and configures the client. Project setup uses .codex/config.toml for Codex or
.mcp.json for Claude Code. --global uses $CODEX_HOME/config.toml
(default ~/.codex/config.toml) or $CLAUDE_CONFIG_DIR/.claude.json
(default ~/.claude.json), with a project-specific server name.
A managed block in AGENTS.md or CLAUDE.md tells the agent when to use Sonar.
Existing settings and instructions are preserved. Identical setup can be repeated;
a same-named connection with different settings needs --replace or another --name.

Reload the client's MCP configuration or start a new session after setup.
The client may require project trust. Ask the agent to call sonar_status to verify.
AI setup guide: https://github.com/BeepBoopBit/sonar-code"#;

pub const STATUS: &str = r#"Examples:
  Report initialization and source freshness:
    sonar status

  Inspect another repository's existing database:
    sonar --db /path/to/repo/.sonar/index.sqlite status

Output is JSON. Before initialization, state is not_initialized and no database
is created. After initialization, check root and index.syntax_freshness, along
with optional navigation, compiler, and embedding preparation.
Refresh saved preparation with sonar init. A stale index is still initialized.
Missing optional model assets are reported without blocking model-free status."#;
