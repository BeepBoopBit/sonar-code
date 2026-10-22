# MCP setup

Sonar serves one prepared repository index over stdio. The agent client launches
the process and reads its navigation results. Indexing, resolution, and embedding
remain explicit CLI operations.

## Set up Codex

With Sonar and Codex CLI installed, run this from the Sonar checkout:

```sh
./dev.sh setup /path/to/your/repo --name sonar-project
```

The helper indexes the repository, prepares source navigation, and registers the
MCP connection after preparation succeeds. Start a new Codex session in the target
repository and check `/mcp`.

This example stores the database at `.sonar/mcp/sonar-project.sqlite` under the
Sonar checkout. Without `--name`, the name is `sonar-<repository-directory-name>`.
Each connection should have its own name and database.

Add optional settings to `setup` as needed:

| Option | Purpose |
| --- | --- |
| `--resolve` | Prepare Rust calls, references, and field uses with rust-analyzer. |
| `--typescript /path/to/node_modules/typescript` | Prepare TypeScript relationships using Node.js and a local compiler. |
| `--model /path/to/model` | Prepare embeddings and enable hybrid search in the MCP server. The model must already exist. |
| `--resources yaml` | Include bounded YAML resources during indexing. |
| `--db /path/to/project.sqlite` | Choose the index location. |
| `--replace` | Explicitly replace an existing named Codex connection. |

Relative paths are resolved from your current directory. See
[optional preparation](usage.md#optional-preparation) for dependencies and model setup.

## Refresh or upgrade

```sh
./dev.sh refresh /path/to/your/repo --name sonar-project
```

Repeat the original preparation options, including `--resolve`, `--typescript`,
`--model`, `--resources`, and any custom `--db`. The helper does not save a refresh
profile. A refreshed index is visible to the running server on its next call;
obtain new handles before continuing navigation.

After changing Sonar's code, run `./dev.sh reinstall` and restart the client
session. Reindex and repeat preparation after upgrades. Changing the configured
database or model also requires updating the connection and restarting the server.

Preparation reports are saved beside the database as `<database>.<step>.json`.

## Configure another client

Prepare an index first:

```sh
sonar --db /absolute/path/to/project.sqlite index /path/to/your/repo
sonar --db /absolute/path/to/project.sqlite index-navigation
```

Configure the client to launch this command:

```sh
sonar --db /absolute/path/to/project.sqlite mcp
```

For a client using `mcpServers` JSON configuration:

```json
{
  "mcpServers": {
    "sonar": {
      "command": "/absolute/path/to/sonar",
      "args": ["--db", "/absolute/path/to/project.sqlite", "mcp"]
    }
  }
}
```

For Codex TOML configuration:

```toml
[mcp_servers.sonar]
command = "/absolute/path/to/sonar"
args = ["--db", "/absolute/path/to/project.sqlite", "mcp"]
startup_timeout_sec = 60
tool_timeout_sec = 60
```

Replace the executable and database paths. `command -v sonar` shows the executable
your shell uses. Absolute paths avoid differences in client working directories
and PATH settings.

For hybrid search, prepare embeddings with the same model and append
`--model /absolute/path/to/model` after `mcp` in the client arguments.

## Tools available to the agent

| Tools | Purpose |
| --- | --- |
| `sonar_status` | Repository root, freshness, languages, and preparation state. |
| `sonar_brief`, `sonar_investigate` | Discover starting points and inspect a selected lead. |
| `sonar_locate`, `sonar_outline` | Find declarations by source position or file. |
| `sonar_neighbors`, `sonar_follow`, `sonar_field_uses` | Inspect recorded relationships and their source evidence. |

All eight tools are read-only. The agent needs its own file-reading tools to
inspect the returned ranges. Paths are relative to the root from `sonar_status`;
follow-up calls take full handles, not response IDs. Replies go to the configured
client. Sonar's query process does not download models or call an external model API.

The server waits for an MCP client on stdin/stdout; it does not expose an HTTP
endpoint. See [troubleshooting](troubleshooting.md#mcp-connection-problems) if a
connection fails.
