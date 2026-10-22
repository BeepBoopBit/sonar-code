# Usage

Install Sonar using the [quick start](../README.md#quick-start). These examples run
from the repository you want to search.

## Index and search

```sh
cd /path/to/your/repo
sonar init
sonar investigate "Where are incoming requests authenticated?"
sonar brief "How are failed requests retried?" --limit 3
```

Initialization prepares the index and basic navigation, updates `.gitignore`,
and saves settings in `.sonar/project.json`. Repeat `sonar init` to refresh saved
options and preparation, including a custom database path. Commands from the
project or its subdirectories reuse those settings.

The default index is `.sonar/index.sqlite` in your project directory. It belongs
to one repository at its absolute path. Save a custom path with
`sonar --db /path/to/project.sqlite init`; later project commands reuse it.
To select a different index or query from outside the project, pass `--db`
explicitly. Existing valid indexes work without a settings file; `init` adopts them.
The lower-level `index ROOT` command remains available for manual workflows.

Commands return JSON. Errors go to stderr and return a nonzero exit status.
`sonar status` works before initialization, reports `initialized: false` and
`state: "not_initialized"`, and creates no database. Search and navigation require
an initialized index. A stale index remains initialized; check freshness separately.
For AI client registration, use `sonar setup codex` or `sonar setup claude`;
see [MCP setup](mcp.md).

| Response field | How to use it |
| --- | --- |
| `investigate`: `panels` and `locations` | Follow the grouped leads to their files, symbols, handles, and suggested `focus` line ranges. |
| `brief`: `starts` and `files` | Read the ranked location IDs in `starts`, then find their details under `files`. |
| `links` | Inspect the recorded relationship and its source evidence. |
| `index` | Check source freshness and each language's capabilities and analysis state. |
| `notices` | Read explanations of missing preparation, stale data, or other limits. |

Open the returned files to verify the behavior. Rankings and term overlap help
choose what to read; an empty result does not establish that a feature is absent.

## Narrow or continue a search

```sh
sonar investigate "How are requests routed?" --scope src --exclude src/generated
sonar outline src/main.rs
sonar locate src/main.rs --line 10
```

Paths are relative to the indexed repository. Adapt the filenames to your project.
`--scope` and `--exclude` take literal paths; repeat `--exclude` for multiple areas.

To inspect a selected lead, copy its full `handle` value:

```sh
sonar investigate "How are requests routed?" --from '<handle>'
```

Use handles from `investigate`, `outline`, `locate`, or `brief --handles`.
Response IDs such as `s1` are not handles. Obtain fresh handles after reindexing.

## Supported files

| Files | Support |
| --- | --- |
| Rust `.rs` | Search and declarations; compiler relationships after `resolve`. |
| Python `.py`, `.pyi` | Search and declarations; resolved calls and field uses are unavailable. |
| TypeScript `.ts`, `.tsx`, `.mts`, `.cts`, including declarations | Search and declarations; compiler relationships after TypeScript resolution. |
| Markdown `.md` | Searchable document sections. |
| YAML `.yaml`, `.yml` | Optional plain-text resources with `init --resources yaml`; maximum 256 KiB per file. |

Languages are detected together. Indexing honors repository `.gitignore` and
`.ignore` rules, skips hidden files and dependency/build directories, and does
not follow symlinks. Parent and global Git ignore rules are not used.

## Optional preparation

Basic `init` includes supported document/resource links and module navigation.
Save additional preparation for later refreshes using these options:

```sh
# Rust calls, references, and field uses:
sonar init --resolve
# TypeScript calls, references, and field uses:
sonar init --typescript /path/to/node_modules/typescript
# Include YAML text resources and select source chunking:
sonar init --resources yaml --chunking ast
```

Options can be combined for a mixed repository. Saved settings include resources,
chunking, Rust and TypeScript preparation, and a local embedding model. Explicit
flags override saved values; later `sonar init` repeats the saved plan.
For manual workflows, `index-navigation` and `resolve` remain separate commands.
Rust resolution needs rust-analyzer, the target's Rust toolchain, and locally
available dependencies; Cargo analysis runs locked and offline. TypeScript
resolution needs Node.js, a local TypeScript 5.x/6.x compiler, and project
dependencies. Configure both language options for a mixed repository.
Recorded relationships are static evidence, and coverage can be incomplete.

For optional embedding search, the downloader below requires a Sonar source
checkout; Cargo installs only the executable. Download a model explicitly, then
prepare vectors:

```sh
python3 /path/to/sonar/scripts/download_model.py --model potion
sonar init --model "$HOME/.sonar/models/potion-code-16M-v2"
sonar investigate "Where are incoming requests authenticated?"
```

The downloader uses Python's standard library and fetches pinned, checksum-verified
assets from Hugging Face. Models default to `~/.sonar/models/potion-code-16M-v2`
or, with `--model minilm`, `~/.sonar/models/all-MiniLM-L6-v2`. These files are
shared across repositories and Sonar checkouts. Repeating the download command
reuses valid files and downloads only missing or damaged assets. Use
`--destination /path/to/model` to override the download location.

`init --model` prepares source embeddings and saves the model path. `investigate`,
`brief`, `batch`, `diagnose`, and MCP reuse it for hybrid search by default.
Use `brief --retrieval lexical` for model-free matching. Model files are used only
for local search; your AI client keeps its own model. Native commands never
download assets automatically.

For a manually maintained index, run `embed --model PATH`, pass the same model
to `investigate` or MCP, and use `brief --retrieval hybrid --model PATH`.
A standalone `embed` or model download does not save project preferences.
Embeddings remain in each repository's index even when models are shared.

## Reuse existing model downloads

Older downloader versions used `.sonar/models/` in the current directory. To
reuse those assets without another download, run from the project or Sonar
checkout containing them:

```sh
mkdir -p "$HOME/.sonar/models"
if [ ! -e "$HOME/.sonar/models/potion-code-16M-v2" ]; then
  cp -R .sonar/models/potion-code-16M-v2 "$HOME/.sonar/models/"
fi
python3 /path/to/sonar/scripts/download_model.py --model potion
```

For MiniLM, copy `all-MiniLM-L6-v2` instead and verify with `--model minilm`.
Update explicit `--model` paths in queries, MCP registrations, and setup/refresh
commands to point to the home copy. Copying identical assets preserves existing
embeddings. You can also keep the old location and continue passing its path.

## Refresh and local data

After source changes, run:

```sh
sonar status
sonar init
```

`init` refreshes the saved preparation plan. MCP clients can call `sonar_init`.
Optional `embed-usage`, `index-quantities`, and imported compiler exports are
separate from this plan; repeat them after a refresh if you use them.

For a manually maintained index, rerun `index` with the same options, then repeat
`index-navigation`, `resolve`, and `embed` as needed. Rerun resolution after
relevant dependency or build-configuration changes, too. There is no watcher.

After upgrading Sonar, rerun `init` and any separately managed preparation.
Briefs omit changed or deleted files while stale; explicit handle navigation
needs current sources.

The SQLite index contains source text and derived data, with possible `-wal` and
`-shm` sidecars. Initialization ignores `.sonar/` and any custom database inside
the repository, including SQLite sidecars.
Indexes, source embeddings, and compiler analysis belong to that repository;
downloaded models are shared in `~/.sonar/models/`.
Initial dependency/model setup can require downloads; indexing and queries use
local files. Queries still need access to the indexed source tree.

See [MCP setup](mcp.md) for agent connections and [troubleshooting](troubleshooting.md)
for recovery steps. Use `sonar <command> --help` for all options.
