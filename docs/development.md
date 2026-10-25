# Development

Use `./dev.sh` from a Bash environment with Rust and Cargo installed. Python 3.8+
is needed for the installer tests. `check` also needs the rustfmt and Clippy
components. The helper reports missing tools with `./dev.sh doctor`.

```sh
./dev.sh doctor
./dev.sh develop
./dev.sh run --help
./dev.sh test
./dev.sh check
```

## Commands

| Command | Behavior |
| --- | --- |
| `help` or no arguments | Show commands and options without preparing anything. |
| `doctor` | Check required development tools and report optional integrations. |
| `install` | Build and install a release executable for your user. |
| `reinstall` | Build and replace the installed executable; also works before a first install. |
| `uninstall` | Remove the executable while keeping indexes, models, and client settings. |
| `develop` | Fetch dependencies using `Cargo.lock`. This may download packages. |
| `run ...` | Build the debug executable and forward arguments to it. |
| `check` | Run Rust formatting checks and Clippy for all targets, treating warnings as errors. |
| `fmt` | Rewrite Rust files using rustfmt. |
| `test ...` | Forward arguments to `cargo test`, then run the Python installer tests. |
| `build` | Build the release executable at `target/release/sonar`. |
| `setup`, `refresh` | Prepare an index and manage its agent connection; see [checkout MCP helper](#checkout-mcp-helper). |

Builds, tests, and checks run in the checkout even when the helper is called from
another directory. `run` executes Sonar in your current directory, so relative
repository and database paths keep their usual meaning.

`install` and `reinstall` always use release builds; the older `--prod` spelling
still works. Installation does not require sudo or modify shell settings. Use
`reinstall` to pick up code changes, then restart running MCP clients.

Native `sonar init` and `sonar setup codex/claude` work from the installed
executable without a checkout. They save settings in the target repository's
`.sonar/project.json` and manage client configuration and project instructions.
Use the [native workflow](mcp.md#automatic-client-setup) for ordinary onboarding.

## Checkout MCP helper

With Sonar and Codex CLI installed, this checkout provides a convenience helper:

```sh
./dev.sh setup /path/to/your/repo --name sonar-project
./dev.sh refresh /path/to/your/repo --name sonar-project
```

`setup` indexes the repository, prepares source navigation, and registers its
Codex connection. Start a new Codex session afterward. `refresh` updates the
index for the existing connection. Cargo-installed users can instead follow
[MCP setup](mcp.md) using only `sonar` commands.

| Option | Purpose |
| --- | --- |
| `--resolve` | Prepare Rust relationships with rust-analyzer. |
| `--typescript /path/to/node_modules/typescript` | Prepare TypeScript relationships with Node.js. |
| `--go` | Prepare Go relationships with a local Go toolchain. |
| `--model /path/to/model` | Prepare embeddings and configure hybrid MCP search. |
| `--resources yaml` | Include bounded YAML resources. |
| `--db /path/to/project.sqlite` | Override the database location. |
| `--replace` | Replace an existing named Codex connection during `setup`. |

By default the database is `.sonar/mcp/<name>.sqlite` under the Sonar checkout,
with preparation reports beside it. Without `--name`, the name is
`sonar-<repository-directory-name>`. Relative paths use your current directory.
Repeat all original preparation options when refreshing; the helper does not
save a refresh profile. See [optional preparation](usage.md#optional-preparation)
for dependencies.

Optional models are shared in `~/.sonar/models/`. Pass that directory explicitly,
for example `--model "$HOME/.sonar/models/potion-code-16M-v2"`, when using the
setup or refresh helper. Model downloads are independent of the database path.

## Tests and configuration

Run a Rust subset while retaining the helper checks:

```sh
./dev.sh test --test exclusions
./dev.sh test --lib
```

To run only the Python helper tests:

```sh
python3 -B -m unittest discover -s scripts -p 'test_*.py'
```

These tests use temporary directories and fake external tools to verify install
failures, argument handling, working directories, and MCP registration behavior.
Model download tests use fixture assets and a mocked network to check sharing
across repositories, reuse, repair, and custom paths without fetching models.

Compiler integration tests use local fixture projects. Go analysis and Rust
analysis run offline; TypeScript resolution uses a locally installed compiler.
The main Go fixtures need Go 1.26+, while `--test go_minimum` uses a Go 1.18 module
to check the analyzer's documented minimum. Rust's real exporter test needs
`rust-analyzer` and `rust-src` from the active Rust toolchain:

```sh
rustup component add rust-analyzer rust-src
SONAR_REQUIRE_COMPILERS=1 \
  SONAR_TEST_GO=/absolute/path/to/go \
  SONAR_TEST_TYPESCRIPT=/absolute/path/to/node_modules/typescript \
  ./dev.sh test
```

Compiler tests skip locally when optional tools are unavailable. Setting
`SONAR_REQUIRE_COMPILERS=1` makes missing tools fail the tests. An explicitly set
compiler path that is unavailable also fails, even without strict mode.

The [CI workflow](../.github/workflows/ci.yml) runs on branch pushes and pull
requests. Both TypeScript 5.9.3 and 6.0.3 run the full suite, including Rust,
Python, TypeScript/TSX, Go, Markdown/YAML, CLI, MCP, and Python helper tests.
CI installs Node.js 24, Go 1.27.3, and Rust's analyzer/source components and
requires every compiler integration. It also checks Go 1.18.10 analysis using
`go_minimum`, Rust 1.88 compilation, formatting, Clippy, and Cargo packaging.
The release workflow reuses these checks before publishing.

| Environment variable | Purpose |
| --- | --- |
| `SONAR_DEV_INSTALL_DIR` | Install directory; defaults to `$CARGO_HOME/bin` or `~/.cargo/bin`. |
| `CARGO_TARGET_DIR` | Build directory; defaults to this checkout's `target/`. |
| `SONAR_PYTHON` | Python executable for installer tests; otherwise tries `python3`, then `python`. |
| `SONAR_REQUIRE_COMPILERS` | Set to `1` to fail when a compiler integration cannot run. |
| `SONAR_TEST_GO` | Go executable for integration tests; otherwise uses `go` on `PATH`. |
| `SONAR_TEST_TYPESCRIPT` | TypeScript package directory; otherwise uses the local `.sonar/typescript/oracle/node_modules/typescript` fixture installation. |
| `SONAR_TEST_RUST_ANALYZER` | Analyzer executable; otherwise uses `rust-analyzer` on `PATH`. |

The install and build directory overrides must be absolute paths. Keep `Cargo.lock`
committed; builds and Cargo tests use `--locked`.

## Project layout

| Path | Contents |
| --- | --- |
| `src/main.rs`, `src/cli_help.rs` | CLI commands, arguments, and usage examples. |
| `src/onboarding.rs`, `src/project.rs` | Saved preparation settings, initialization, safe client configuration, and managed instructions. |
| `src/lib.rs`, `src/` | Index storage, retrieval, ranking, and navigation. |
| `src/languages/` | Rust, Python, TypeScript, and Go adapters and compiler integration. |
| `src/mcp.rs`, `src/mcp/` | MCP tool definitions and runtime. |
| `tests/` | Rust integration tests; additional unit tests live beside source code. |
| `scripts/` | Model downloads, embedding verification, and helper tests. |
| `docs/` | Usage, MCP, troubleshooting, and development guides. |

The optional `scripts/check_embeddings.py` compares embeddings against Python
reference implementations. It requires a compatible Rust encoder executable via
`--binary`; that helper is not included in this checkout. Its Python dependencies
are listed in the two reference requirements files under `scripts/`. It defaults
to the same shared Potion model in `~/.sonar/models/potion-code-16M-v2`; use
`--model` for a different local model.

Reference environments, models, indexes, build output, and historical evaluation
data stay outside Git. See [usage](usage.md#optional-preparation) for model downloads.
