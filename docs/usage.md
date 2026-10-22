# Usage

Install Sonar using the [quick start](../README.md#quick-start). These examples run
from the repository you want to search.

## Index and search

```sh
cd /path/to/your/repo
sonar index .
sonar investigate "Where are incoming requests authenticated?"
sonar brief "How are failed requests retried?" --limit 3
```

The default index is `.sonar/index.sqlite` in your current directory. It belongs
to one repository at its absolute path. To keep multiple indexes, pass
`--db /path/to/project.sqlite` to every command for that project.

Queries return JSON. Errors go to stderr and return a nonzero exit status.

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
| YAML `.yaml`, `.yml` | Optional plain-text resources with `index . --resources yaml`; maximum 256 KiB per file. |

Languages are detected together. Indexing honors repository `.gitignore` and
`.ignore` rules, skips hidden files and dependency/build directories, and does
not follow symlinks. Parent and global Git ignore rules are not used.

## Optional preparation

Basic search needs only `index`. For richer navigation, prepare the features you use:

```sh
sonar index-navigation
# Rust calls, references, and field uses:
sonar resolve
# TypeScript calls, references, and field uses:
sonar resolve --typescript /path/to/node_modules/typescript
```

`index-navigation` adds supported document/resource links and module navigation.
Rust resolution needs rust-analyzer, the target's Rust toolchain, and locally
available dependencies; Cargo analysis runs locked and offline. TypeScript
resolution needs Node.js, a local TypeScript 5.x/6.x compiler, and project
dependencies. Run both resolution commands for a mixed Rust/TypeScript repository.
Recorded relationships are static evidence, and coverage can be incomplete.

For optional embedding search, download a model explicitly, then prepare vectors:

```sh
python3 /path/to/sonar/scripts/download_model.py --model potion \
  --destination .sonar/models/potion-code-16M-v2
sonar embed --model .sonar/models/potion-code-16M-v2
sonar investigate "Where are incoming requests authenticated?" \
  --model .sonar/models/potion-code-16M-v2
```

The downloader uses Python's standard library and fetches pinned, checksum-verified
assets from Hugging Face. `investigate --model` combines lexical and embedding
search. For `brief`, also supply `--retrieval hybrid`.

## Refresh and local data

After source changes, rerun `index` with the same options, followed by whichever
preparation commands you use: `index-navigation`, `resolve`, and `embed`. Repeat
options such as `--resources yaml` when indexing. Rerun resolution after relevant
dependency or build-configuration changes, too. There is no automatic watcher.

After upgrading Sonar, reindex and repeat optional preparation. Briefs omit changed
or deleted files while stale; explicit handle navigation needs current sources.

The SQLite index contains source text and derived data, with possible `-wal` and
`-shm` sidecars. Add `.sonar/` to the target repository's `.gitignore` if needed.
Initial dependency/model setup can require downloads; indexing and queries use
local files. Queries still need access to the indexed source tree.

See [MCP setup](mcp.md) for agent connections and [troubleshooting](troubleshooting.md)
for recovery steps. Use `sonar <command> --help` for all options.
