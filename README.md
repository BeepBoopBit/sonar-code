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
sonar index .
sonar investigate "Where are incoming requests authenticated?"
```

The index is saved to `.sonar/index.sqlite` in your current directory. Run queries from the same directory, and rerun `sonar index .` after source changes.

Use `sonar --db /path/to/index.sqlite <command>` to select a different index. Each index belongs to one repository.

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

For caller and reference navigation, prepare Rust relationships with `sonar resolve` (requires rust-analyzer and project dependencies). For TypeScript, use `sonar resolve --typescript /path/to/node_modules/typescript` (requires Node.js and project dependencies).

Run `sonar --help` or `sonar <command> --help` for all options.

## Use with coding agents

Agents with shell access can use the CLI commands above. For MCP clients, prepare
an index and configure the client to launch Sonar over stdio:

```sh
sonar --db /absolute/path/to/index.sqlite mcp
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

Local indexes, models, build output, and historical evaluation data are gitignored.

## Documentation

- [Usage](docs/usage.md) — search, results, supported files, and index updates.
- [Agent setup](docs/agents.md) — install with an agent and guide its use of Sonar.
- [MCP setup](docs/mcp.md) — connect agents and refresh their indexes.
- [Troubleshooting](docs/troubleshooting.md) — common errors and recovery steps.
- [Development](docs/development.md) — helper commands, tests, and project layout.
- [Releasing](docs/releasing.md) — package checks and crates.io publication.

## License

[MIT](LICENSE) © 2026 beepboopbit.
