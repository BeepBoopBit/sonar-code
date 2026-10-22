# Troubleshooting

Check `sonar --version` and `sonar --help`. From a Sonar checkout,
`./dev.sh doctor` also checks development tools. For query problems, inspect the
JSON `index` and `notices` fields, or run `sonar status`; in an MCP client,
call `sonar_status`.

## Initialization or client setup

Run `sonar init`, or `sonar setup codex` / `sonar setup claude` for a complete
client connection. MCP clients can call `sonar_init` if status reports
`initialized: false`. Initialization saves custom database paths and optional
preparation settings; repeat it after changes to refresh the same plan.

Setup preserves other client entries and instructions. A conflicting connection
needs `--replace` or a distinct `--name`. Malformed client configuration and
incomplete managed instruction blocks must be corrected before changes can be
merged. Reload the client after setup, accept project trust if required, then
call `sonar_status`. The verification checks the local process, not whether a
running client session has reloaded its configuration.

## Command or index not found

If `sonar` is missing from PATH, use the install destination printed by Cargo or
`./dev.sh install`. The default is `$CARGO_HOME/bin`, or `~/.cargo/bin` when
`CARGO_HOME` is unset. Add that directory to your shell's PATH.

Rerun `cargo install sonar-code --locked` to update. For a local checkout, use
`./dev.sh reinstall`. The package is named
`sonar-code`, but the installed command is `sonar`.

If the index is missing, select the same database when indexing and querying:

```sh
sonar --db /absolute/path/to/project.sqlite init /path/to/your/repo
sonar --db /absolute/path/to/project.sqlite brief "request authentication"
```

Later commands inside the project reuse its saved database path. From elsewhere,
pass `--db` explicitly. Without saved settings or `--db`, the default is
`.sonar/index.sqlite` under the current directory. Helper-created MCP indexes live
under the checkout's `.sonar/mcp/`; those helpers do not save native settings.

## Stale sources, handles, or a moved repository

Run `sonar init` or call `sonar_init` to repeat saved preparation, then repeat
separately managed steps such as caller vectors or quantities if used. For manual
indexes, repeat `index` and preparation with the original flags. Obtain new handles;
short response IDs such as `s1` cannot be used for navigation.

Both the index and `.sonar/project.json` are bound to the repository's absolute
path. After a move, stop its MCP processes, inspect the saved paths, and move
`.sonar/project.json` aside to an unused backup path. Then create a new database:

```sh
sonar --db /new/path/to/repo/.sonar/rebuilt.sqlite init /new/path/to/repo
```

Repeat saved `--resources`, `--chunking`, `--resolve`, `--typescript`, and `--model`
options you want to retain. Check status, rerun `sonar setup codex --replace` or
the Claude equivalent from the new root, and reload the client. Global setup uses
a root-derived server name; remove the old connection separately. For a different
repository, always choose a separate database.

Initialization upgrades supported older schemas through indexing. If it still
reports `unsupported index database`, select a fresh `--db` path and run init again.

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
that do not cover the current generation, rerun `sonar init` if its model is
saved, or `embed --model /path/to/model` against the same database after indexing.
Use the matching model for queries and MCP. The [model downloader](../scripts/download_model.py) can fetch missing assets;
queries do not download them automatically.

The downloader defaults to `~/.sonar/models/`, shared across repositories.
If models were previously downloaded into a project's `.sonar/models/`, pass
that existing path or [copy them to the shared location](usage.md#reuse-existing-model-downloads)
to avoid downloading them again. Save that path with `sonar init --model PATH`,
or repeat it on embedding, queries, and MCP for manual setups. Status remains
available when optional model assets are missing and reports the model error.

## MCP connection problems

Confirm the configured executable and root are accessible using absolute paths.
The database can be created through `sonar_init`; query/navigation calls require
initialization. Check that the client can also read the source tree.
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
