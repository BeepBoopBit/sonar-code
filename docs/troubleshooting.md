# Troubleshooting

Check `sonar --version` and `sonar --help`. From a Sonar checkout,
`./dev.sh doctor` also checks development tools. For query problems, inspect the
JSON `index` and `notices` fields; in an MCP client, call `sonar_status`.

## Command or index not found

If `sonar` is missing from PATH, use the install destination printed by Cargo or
`./dev.sh install`. The default is `$CARGO_HOME/bin`, or `~/.cargo/bin` when
`CARGO_HOME` is unset. Add that directory to your shell's PATH.

Rerun `cargo install sonar-code --locked` to update. For a local checkout, use
`./dev.sh reinstall`. The package is named
`sonar-code`, but the installed command is `sonar`.

If the index is missing, select the same database when indexing and querying:

```sh
sonar --db /absolute/path/to/project.sqlite index /path/to/your/repo
sonar --db /absolute/path/to/project.sqlite brief "request authentication"
```

Without `--db`, the path is `.sonar/index.sqlite` relative to your working directory.
Helper-created MCP indexes normally live under the Sonar checkout's `.sonar/mcp/`.

## Stale sources, handles, or a moved repository

Rerun `index` with the original options, then repeat the optional navigation,
resolution, and embedding steps you use. Obtain new handles afterward. Use a
location's `handle` value; short IDs such as `s1` cannot be used for navigation.

An index is bound to its repository's absolute path. If you move the repository
or see `this database belongs to ...`, choose a new `--db` path and rebuild the
index. Update any MCP connection to use the new path.

For an older database schema, `sonar index` upgrades supported versions. If it
still reports `unsupported index database`, create a new index and prepare it again.

## Empty or incomplete results

- Confirm the index points to the intended repository and reports current sources.
- Check [supported files and ignore rules](usage.md#supported-files). Ignored files,
  hidden paths, symlinks, and unsupported extensions are outside the index.
- Remove restrictive `--scope` or `--exclude` options and try a known symbol name.
- If calls or references are missing, prepare resolution for that language.
  Python supports declarations and structural navigation, without resolved calls.

`sonar diagnose "your query"` exposes candidate and ranking details. Empty results
can reflect retrieval or coverage limits.

## Resolution or embedding errors

Rust resolution needs rust-analyzer, the repository's Rust toolchain, and its
dependencies already available locally. If Cargo reports missing dependencies,
fetch them explicitly before retrying:

```sh
cargo fetch --locked --manifest-path /path/to/your/repo/Cargo.toml
sonar --db /absolute/path/to/project.sqlite resolve
```

Rust resolution runs Cargo locked and offline. Inspect `resolve.log` beside the
database for analysis failures. TypeScript resolution needs Node.js, an installed
TypeScript 5.x/6.x compiler module, and the target project's dependencies; pass the
compiler path with `--typescript`.

For `index model does not match these assets`, incomplete embeddings, or embeddings
that do not cover the current generation, rerun `embed --model /path/to/model`
against the same database after indexing. Use that same model for queries and
MCP. The [model downloader](../scripts/download_model.py) can fetch missing assets;
queries do not download them automatically.

## MCP connection problems

Confirm the configured executable and database exist, using absolute paths. Check
that the client's execution environment can also read the indexed repository.
Inspect its error output, and start a new session after setup or upgrading.

For Codex, `codex mcp get sonar-project` shows the registered command. If
`sonar_status` reports the wrong root, select that project's database in the client
configuration. Each repository needs a separate index.

If you use the checkout's `dev.sh` helper, an existing connection name requires
`setup --replace`; use `refresh` when only updating the index. Repeat all original
preparation flags on refresh.

Running `sonar mcp` directly can appear idle because it waits for protocol messages
on stdin. It is not a terminal prompt or web server. See [MCP setup](mcp.md).

## Tests refer to an old checkout path

Cached Rust test binaries can retain paths from before a repository move. Rebuild
Sonar's debug artifacts from the current checkout:

```sh
cargo clean --package sonar-code --profile dev
./dev.sh test
```

This removes generated package build artifacts; source files and indexes remain.
