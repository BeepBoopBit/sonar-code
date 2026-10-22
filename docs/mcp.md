# MCP setup

Sonar binds one repository to an MCP stdio server. The client launches the
installed `sonar` executable; no checkout is needed. Status and initialization
work before an index exists. Search and navigation require an initialized index
and remain read-only; the explicit `sonar_init` tool initializes or refreshes it.

## Automatic client setup

From the repository you want to navigate:

```sh
sonar setup codex
# Or:
sonar setup claude
```

Setup initializes the repository and basic navigation, updates `.gitignore`,
verifies the local MCP process, and merges client settings. It adds a managed
instruction block to `AGENTS.md` or `CLAUDE.md`. Project registration uses
`.codex/config.toml` for Codex or `.mcp.json` for Claude Code. Existing servers,
settings, and instructions are preserved; identical setup can be repeated.
Use `--replace` for a conflicting registration, `--name NAME` for a different
entry, or `--global` for user configuration. The project server name defaults
to `sonar`. Global setup uses a project-specific name derived from the root and
writes `$CODEX_HOME/config.toml` (default `~/.codex/config.toml`) or
`$CLAUDE_CONFIG_DIR/.claude.json` (default `~/.claude.json`). Managed instructions
stay in the project even with `--global`.

For example:

```sh
sonar setup codex /path/to/repo --resolve --name project-code
sonar setup claude /path/to/repo --global
```

Reload the client or start a new session; project trust may be required. The
verification checks the local MCP process and does not reload a running client.

Basic setup needs no model or compiler. Optional `--resolve`, `--typescript`,
`--model`, `--resources`, and `--chunking` are saved in `.sonar/project.json`.
Repeating `sonar init` refreshes that saved plan. Model assets must already be local.

## Initialize through MCP

Bind a repository before its database exists:

```sh
sonar --db /absolute/path/to/repo/.sonar/index.sqlite mcp --root /absolute/path/to/repo
```

Call `sonar_status` to inspect `initialized` and `state`, then call `sonar_init`
with `{}` if needed. Status and startup create no database. Initialization indexes
supported files, prepares basic navigation, adds the ignore rule, and saves settings.
It can repeat saved compiler/model preparation, but cannot change the bound root
or client configuration. Query/navigation tools return `not_initialized` until
ready and remain read-only. Existing valid indexes need no separate marker.

After source changes, run `sonar init` or call `sonar_init` to refresh saved
preparation. Obtain new handles afterward; a stale index remains initialized.

## Prepare an index

Install with `cargo install sonar-code --locked`, then run from the repository
you want the agent to navigate:

```sh
cd /path/to/your/repo
sonar init
```

This creates `.sonar/index.sqlite`, prepares basic navigation, updates ignore
rules, and saves settings. Optional `init --model PATH` saves an existing model
for embedding preparation and hybrid search; assets are shared in
`~/.sonar/models/` when using the checkout downloader. For calls and references,
see [optional preparation](usage.md#optional-preparation).

## Set up Codex

For manual registration instead of `sonar setup codex`, install Codex CLI and
run from the prepared repository:

```sh
codex mcp add sonar-project -- "$(command -v sonar)" \
  --db "$PWD/.sonar/index.sqlite" mcp --root "$PWD"
codex mcp get sonar-project
```

Choose a different connection name for each repository. Inspect an existing entry
with `codex mcp get` before changing it. Start a new Codex session and check `/mcp`.
Ask the agent to call `sonar_status` and confirm that `root` is the intended
repository and `index.syntax_freshness` is `current`.

You can also add this table to your Codex `config.toml`, replacing the paths.
`--root` binds the project even if its database needs initialization:

```toml
[mcp_servers.sonar-project]
command = "/absolute/path/to/sonar"
args = ["--db", "/absolute/path/to/your/repo/.sonar/index.sqlite", "mcp", "--root", "/absolute/path/to/your/repo"]
startup_timeout_sec = 60
tool_timeout_sec = 300
```

See the [official Codex MCP documentation](https://learn.chatgpt.com/docs/extend/mcp?surface=cli)
for configuration locations and client settings.

## Configure another client

For clients using `mcpServers` JSON configuration, add an entry like this to the
client's MCP settings, preserving other entries:

```json
{
  "mcpServers": {
    "sonar-project": {
      "command": "/absolute/path/to/sonar",
      "args": ["--db", "/absolute/path/to/your/repo/.sonar/index.sqlite", "mcp", "--root", "/absolute/path/to/your/repo"]
    }
  }
}
```

Replace the executable, database, and root paths. `command -v sonar` shows the
executable your shell uses. The executable and source tree must be accessible in
the client's environment; the database may be created through `sonar_init`.
The client launches this command:

```sh
sonar --db /absolute/path/to/your/repo/.sonar/index.sqlite mcp --root /absolute/path/to/your/repo
```

Use a client that can launch local stdio servers. Sonar does not expose an HTTP
endpoint, and running `sonar mcp` in a terminal simply waits for protocol messages.

A model saved by `init --model PATH` is reused for hybrid discovery. For a manual
index without saved settings, prepare embeddings and append the same
`--model /absolute/path/to/model` after `mcp` in the client arguments.

## Refresh or upgrade

After source changes, run from the indexed repository:

```sh
sonar init
```

This repeats saved indexing, navigation, compiler, and source embedding settings.
MCP clients can call `sonar_init` instead. Separately managed features such as
caller vectors and quantities still need their own preparation commands.
For a manual index without saved settings, repeat `index` and the preparation
commands with their original flags. The running server sees the refreshed index
on its next call; obtain new handles.

Update the executable with `cargo install sonar-code --locked`, run `sonar init`
and any separately managed preparation, then restart the client. When changing
the database or model, rerun native setup with `--replace` or update the manual
registration and restart the server.

## Tools available to the agent

| Tools | Purpose |
| --- | --- |
| `sonar_init` | Initialize or refresh the bound repository and saved preparation. |
| `sonar_status` | Repository root, freshness, languages, and preparation state. |
| `sonar_brief`, `sonar_investigate` | Discover starting points and inspect a selected lead. |
| `sonar_locate`, `sonar_outline` | Find declarations by source position or file. |
| `sonar_neighbors`, `sonar_follow`, `sonar_field_uses` | Inspect recorded relationships and their source evidence. |

Start with `sonar_status`, call `sonar_init` if needed, then search with
`sonar_investigate` or `sonar_brief`. Read the returned source ranges using the
agent's file tools. Paths are relative to the root from `sonar_status`;
follow-up calls take full handles, not response IDs. Sonar's query process does
not download models or call an external model API.

See [agent instructions](agents.md#instructions-for-everyday-use) for a reusable
workflow and [troubleshooting](troubleshooting.md#mcp-connection-problems) for
connection failures. Checkout users can also use the
[development helper](development.md#checkout-mcp-helper).
