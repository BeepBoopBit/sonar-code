# MCP setup

Sonar serves one prepared repository index over stdio. The client launches the
installed `sonar` executable; no Sonar checkout is needed. Indexing, resolution,
and embedding are explicit CLI operations, while all MCP tools are read-only.

## Prepare an index

Install with `cargo install sonar-code --locked`, then run from the repository
you want the agent to navigate:

```sh
cd /path/to/your/repo
sonar index .
sonar index-navigation
```

This creates `.sonar/index.sqlite` under that repository. Keep `.sonar/` gitignored.
For calls and references, also run the relevant
[optional preparation](usage.md#optional-preparation).

## Set up Codex

With Codex CLI installed, run from the same indexed repository:

```sh
codex mcp add sonar-project -- "$(command -v sonar)" \
  --db "$PWD/.sonar/index.sqlite" mcp
codex mcp get sonar-project
```

Choose a different connection name for each repository. Inspect an existing entry
with `codex mcp get` before changing it. Start a new Codex session and check `/mcp`.
Ask the agent to call `sonar_status` and confirm that `root` is the intended
repository and `index.syntax_freshness` is `current`.

You can also add this table to your Codex `config.toml`, replacing both paths:

```toml
[mcp_servers.sonar-project]
command = "/absolute/path/to/sonar"
args = ["--db", "/absolute/path/to/your/repo/.sonar/index.sqlite", "mcp"]
startup_timeout_sec = 60
tool_timeout_sec = 60
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
      "args": ["--db", "/absolute/path/to/your/repo/.sonar/index.sqlite", "mcp"]
    }
  }
}
```

Replace the executable and database paths. `command -v sonar` shows the executable
your shell uses. Both paths and the indexed source tree must exist in the client's
execution environment. The client launches this command:

```sh
sonar --db /absolute/path/to/your/repo/.sonar/index.sqlite mcp
```

Use a client that can launch local stdio servers. Sonar does not expose an HTTP
endpoint, and running `sonar mcp` in a terminal simply waits for protocol messages.

For hybrid search, prepare embeddings with the same model and append
`--model /absolute/path/to/model` after `mcp` in the client arguments.

## Refresh or upgrade

After source changes, run from the indexed repository:

```sh
sonar index .
sonar index-navigation
```

Repeat the original indexing options and any resolution or embedding steps you
use. If you chose a custom database, pass its `--db` path to each command. The
running server sees the refreshed index on its next call; obtain new handles.

Update the executable with `cargo install sonar-code --locked`, reindex, repeat
optional preparation, and restart the client. Changing the configured database
or model also requires updating the connection and restarting the server.

## Tools available to the agent

| Tools | Purpose |
| --- | --- |
| `sonar_status` | Repository root, freshness, languages, and preparation state. |
| `sonar_brief`, `sonar_investigate` | Discover starting points and inspect a selected lead. |
| `sonar_locate`, `sonar_outline` | Find declarations by source position or file. |
| `sonar_neighbors`, `sonar_follow`, `sonar_field_uses` | Inspect recorded relationships and their source evidence. |

Read the returned source ranges using the agent's file tools. Paths are relative
to the root from `sonar_status`; follow-up calls take full handles, not response
IDs. Sonar's query process does not download models or call an external model API.

See [agent instructions](agents.md#instructions-for-everyday-use) for a reusable
workflow and [troubleshooting](troubleshooting.md#mcp-connection-problems) for
connection failures. Checkout users can also use the
[development helper](development.md#checkout-mcp-helper).
