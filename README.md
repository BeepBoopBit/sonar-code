# Sonar

Local code navigation for coding agents. Sonar finds relevant files, symbols, and line ranges, and returns them as JSON.

Supports **Rust, Python, TypeScript/TSX, and Markdown**. Basic search runs locally without a model or API key.

## Quick start

Requires Rust 1.88+ and native build tools (a C/C++ compiler and linker).
Install from crates.io:

```sh
cargo install sonar-code --locked
```

The executable is named `sonar`:

```sh
cd /path/to/your/repo
sonar init
sonar investigate "Where are incoming requests authenticated?"
```

Initialization adds `.sonar/` to `.gitignore` and saves settings in
`.sonar/project.json`. The default database is `.sonar/index.sqlite`. Commands
from the project or its subdirectories reuse saved paths. Rerun `sonar init`
after source changes to refresh the saved preparation plan.

Pass `--db /path/to/index.sqlite` to `init` to save a custom database path.
On later commands, `--db` overrides the saved path. Each index belongs to one repository.

Optional embedding models are downloaded once into `~/.sonar/models/` and shared
across repositories. See [model setup and existing downloads](docs/usage.md#optional-preparation).

From a development checkout, `./dev.sh install` and `./dev.sh reinstall` also build
and install the command. Cargo installs the executable; `dev.sh` and the optional
Python helpers require the checkout.

## Common commands

```sh
# Get a short list of places to start reading.
sonar brief "How are failed requests retried?"

# List the declarations in an indexed file.
sonar outline src/main.rs

# Narrow a search to a directory and skip an irrelevant area.
sonar investigate "How are requests routed?" --scope src --exclude src/generated
```

For caller and reference navigation, use `sonar init --resolve` for Rust
(requires rust-analyzer and local project dependencies), or
`sonar init --typescript /path/to/node_modules/typescript` for TypeScript
(requires Node.js and project dependencies). These options are saved for refreshes.
For embedding search, `sonar init --model /path/to/model` saves an existing local
model and prepares vectors; project searches then default to hybrid retrieval.

Run `sonar --help` or `sonar <command> --help` for all options.

## Use with coding agents

Initialize and connect your current project in one command:

```sh
sonar setup codex
# Or:
sonar setup claude
```

Setup verifies the local MCP server, preserves other client settings, and adds a
managed instruction block to `AGENTS.md` or `CLAUDE.md`. Project configuration
uses `.codex/config.toml` or `.mcp.json`. Use `--global` for user configuration,
`--name NAME` for a distinct server entry, or `--replace` for a conflicting entry.
Reload the client and call `sonar_status` after setup. Basic initialization needs
no model or compiler.

Agents with shell access can also use the CLI directly. Manual MCP clients can
launch Sonar over stdio:

```sh
sonar --db /absolute/path/to/repo/.sonar/index.sqlite mcp --root /absolute/path/to/repo
```

See [agent setup](docs/agents.md) for a setup prompt and reusable agent
instructions, or [MCP setup](docs/mcp.md) for Codex, JSON, and TOML configuration.
These workflows use the published CLI and do not require a Sonar checkout.

## Development

```sh
./dev.sh doctor
./dev.sh develop
./dev.sh run --help
./dev.sh test
./dev.sh check
```

`run` uses a debug build and keeps your current directory. `test` runs Rust and installer tests (Python 3.8+ required). Use `fmt` to format Rust code and `build` for a release executable. Run `./dev.sh` to see all commands.

Project indexes, build output, and historical evaluation data are gitignored.
Shared model downloads live in your home directory.

## Documentation

- [Usage](docs/usage.md) — search, results, supported files, and index updates.
- [Agent setup](docs/agents.md) — install with an agent and guide its use of Sonar.
- [MCP setup](docs/mcp.md) — connect agents and refresh their indexes.
- [Troubleshooting](docs/troubleshooting.md) — common errors and recovery steps.
- [Development](docs/development.md) — helper commands, tests, and project layout.
- [Releasing](docs/releasing.md) — package checks and crates.io publication.

## License

[MIT](LICENSE) © 2026 beepboopbit.
