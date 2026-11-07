# Dependency analysis

Sonar can explain connections between indexed files without an LLM or embedding
model. Refresh an existing index with `sonar init` or `sonar index .` to prepare
the dependency facts. Older indexes require this explicit refresh.

```sh
sonar dependencies src/lib.rs
sonar dependents src/store.rs --depth 2
```

`dependencies` follows outgoing relationships; `dependents` follows incoming
relationships. Both return JSON. `--depth` accepts 1–4 (default 1); `--limit`
accepts 1–1000 (default 100) and sets the number of **file-pair groups per page**.
Repeated occurrences between the same files do not consume additional groups.

MCP exposes the same query as `sonar_dependencies`, with `path`, optional
`direction` (`outgoing`, `incoming`, or `both`, default `outgoing`), `depth`,
`limit`, and `cursor`. The Rust API is `sonar::analysis::dependencies`.

## Grouped results and continuation

Dependency responses use `version: 2`. Each relationship preserves the original
`from` and `to` direction and summarizes its kinds separately:

```json
{
  "from": "src/handler.rs",
  "to": "src/store.rs",
  "kinds": [{
    "kind": "calls",
    "occurrences": 27,
    "evidence": [{
      "path": "src/handler.rs", "line": 12, "bytes": [240, 244],
      "provider": "rust:example-profile"
    }],
    "evidence_truncated": true
  }]
}
```

Each kind includes up to three source examples with half-open UTF-8 byte ranges.
`occurrences` counts distinct source-bound facts after duplicate removal;
`evidence_truncated` indicates examples were omitted. Counts describe static
occurrences, not execution frequency. Reverse traversal does not reverse facts.

When `next_cursor` is present, pass it unchanged with the **same path, direction,
depth, and limit**. For example:

```sh
sonar dependents src/store.rs --limit 20
sonar dependents src/store.rs --limit 20 --cursor '<returned next_cursor>'
```

Cursors bind the normalized query, repository/index generation, analysis
freshness/profiles, and result identity. Changed source, relevant compiler
inputs, new analysis, or different options invalidate the cursor; restart the
query. Equivalent normalized paths such as `./src/store.rs` are accepted.

- `relationships_total` counts all reachable file-pair groups within depth.
  Pagination follows complete bounded-depth traversal, so page size does not
  change which transitive connections are discoverable.
- `files` contains the anchor and paths used by the current page's groups and
  unresolved records. `reachable_files` counts all files reached before paging.
- `unresolved` lists dependency syntax from all reached files whose target is
  unknown. `unresolved_total` counts these records before paging. An incoming
  query cannot attribute unknown targets in other files to the anchor.
- `coverage` summarizes repository-wide syntax providers and limitations.
  `analyzed_files` and `skipped_total` are counts; `skipped_files` is paginated.
  `index.languages` reports compiler capability and freshness; `notices` reports
  missing syntax and semantic analysis.
- Each page returns up to `limit` groups, unresolved records, and skipped paths
  per provider **independently**. Following all cursors retrieves each stream
  without repetition. Later pages may contain only auxiliary records after the
  groups are exhausted. The query requires no server-side pagination session.
- `truncated` means more records are available through `next_cursor`.
  `depth_limited` means further file connections exist beyond the requested
  hop boundary. Evidence sampling is reported separately by each kind.

This replaces the initial version-1 flat occurrence output. JSON consumers must
read `relationships[].kinds[]`; Rust callers constructing `DependencyOptions`
must supply `cursor` or use `..Default::default()`.

Queries require an unchanged indexed source snapshot and never refresh or write
analysis implicitly. Static relationships identify code to inspect; they do not
prove execution flow, measured test coverage, or a defect. Empty output does not
prove the absence of dependencies.

## Initial support

The Rust syntax provider resolves ordinary out-of-line module declarations,
including declarations inside inline modules. Imports and re-exports are indexed
as individual original-name leaves, including grouped imports and aliases.
At query time, exact current compiler occurrences can resolve each leaf to an
indexed declaration's file. The language provider refines shared compiler facts
without database access or a second Rust name resolver. Source evidence identifies
the original imported name, not the alias or a module prefix.

Missing/ambiguous bindings and glob expansions remain unresolved independently;
one resolved name does not resolve its siblings. External targets absent from the
index remain unresolved. Same-file imports resolve without adding self edges.
Source syntax batches remain cached; refinement is read-only and disappears
when compiler freshness is lost. Missing or ambiguous module targets and
attribute-driven layouts also remain unresolved. Files with parse errors are
listed as skipped. This does not expand macros, evaluate `cfg`, or resolve
`#[path]` attributes. Module links describe indexed source layout, not active build
configuration.

Current compiler-resolved calls, references, and implementation relationships
are projected onto file dependencies through Sonar's existing semantic providers
(Rust, TypeScript, Go). This does not introduce syntax dependency providers for
TypeScript or Go. External/unindexed compiler targets are not included;
the graph is not a complete package dependency inventory.

### Python local imports

The `python-dependencies-v1` syntax provider records module-layout inspection leads
from `import` and `from ... import ...`, including aliases and multiline/grouped
imports. It uses only indexed sources and never invokes Python or imports project
code. Absolute imports consider the repository root and conventional `src/` as
candidate search roots; relative imports follow contiguous regular packages with
`__init__.py`. Multiple observed roots, module/package collisions, namespace
packages, stub-only targets, missing/external targets, and relative imports outside
the supported package context remain unresolved. A `.py` source takes precedence
over its `.pyi` companion as the runtime-layout lead.

Dotted imports retain indexed package initializers as well as the target module.
`from module import name` links the named module without claiming to bind `name`.
For `from package import child`, a uniquely indexed child can be selected when the
package initializer is passive (empty, comments, docstrings, or `pass`). A package
attribute, re-export, or dynamic export can otherwise precede submodule loading;
uncertain members remain explicit gaps while the package initializer is still
linked. Same-file links are omitted.

Wildcard imports retain a known base-module link but report unexpanded names.
Recognizable `__import__`, `importlib.import_module`, and direct importlib aliases
are reported as unsupported dynamic imports, even with literal arguments. Future
imports are compiler directives, not local file dependencies. Files with syntax
parse errors are skipped and reported. Comments and strings are not imports.

These are source-layout leads, not a Python runtime resolver. Arbitrary source
roots/packaging configuration, `sys.path`/`__path__` mutation, import hooks,
built-in/frozen-module precedence, conditional execution, and symbol rebinding
are not evaluated. Dynamic calls through arbitrary aliases may be missed. No
compiler-backed Python callers or symbol-reference graph is added.

Existing indexes must be refreshed once with `sonar index <root>` because the
provider set changed; queries reject old provider batches rather than silently
omitting imports. Shared queries, reverse dependencies, review areas, impact
analysis, and review packs consume the same persisted facts. Module-level reading
paths retain a header range to include file contracts/imports. This range-selection
change uses review policy `review-clusters-v2`; regenerate older review identities
and continuations. Provider coverage also participates in analysis fingerprints.

Baseline-aware syntax and optional compiler comparisons are described below.
Semantic equivalence, full control/data flow, and LLM review reports remain
outside this foundation.

## Architecture

`src/analysis` owns typed facts, provider input/output contracts, validation,
persistence, and traversal. Language adapters optionally supply a dependency
provider. Providers receive indexed sources and return an `AnalysisBatch`; they
have no database access. Existing indexed files and declarations remain the
source of entity identity; this layer does not create a second symbol index.

Syntax batches are stored transactionally with the source index, keyed by a
versioned provider identity, shared fact policy, and source snapshot. Unchanged
snapshots reuse provider results. A changed snapshot rebuilds syntax dependency
facts conservatively. Existing compiler freshness/invalidation rules still apply.
Dependency queries currently read the available graph into memory; output pagination
bounds response size, not total graph loading or analysis cost.

File call/reference relationships are derived from current semantic edges at
query time rather than copied into another independent graph.

To add language support, implement `AnalysisProvider`, expose it through the
language adapter's `dependency_provider`, and add source fixtures. Shared
persistence, reverse traversal, limits, CLI, and MCP need no language-specific
branches. Increment the provider identity when extraction semantics change.

## Review clusters

Two read-only workflows share the same graph and selection engine:

```sh
sonar review-context crates/runtime/src/coordinator.rs --max-files 12
sonar review-context exact_symbol --max-files 8
sonar review-areas --max-files 12 --limit 5
```

MCP exposes `sonar_review_context` (`seed`, `max_files`, `scope`) and
`sonar_review_areas` (`max_files`, `scope`, `limit`, `cursor`). Library callers use
`review_context` / `ReviewContextOptions` and `review_areas` / `ReviewAreasOptions`.
Both responses report `version: 1` and a versioned selection `policy`.

### Selection and reading paths

`review-context` accepts an indexed code-file path, an unambiguous exact symbol,
or a full current navigation handle from `locate`/`outline`. Symbol and handle
seeds select around their containing file and retain the declaration's source
range; this is file-level clustering, not a symbol-specific program slice.
Ambiguous names require a handle. The selected seed must be inside `--scope`.

`--max-files` is a hard budget of 1–50 selected files (default 12), including
selected test files. Selection expands along recorded relationships in either
direction and may stop before filling the budget. Calls receive more weight
than references or module declarations; occurrence contributions are capped,
and shared directory prefixes favor local connections. Scores are deterministic
prioritization heuristics. They are not calibrated confidence or risk scores.

Each cluster includes:

- `id`, a snapshot-bound identity for its selected file set, and `title`, a
  structural directory label. Titles are not inferred feature names.
- `files` and `reading_path`, in selection order. Each non-seed reading step has
  an earlier `parent_file` and a `via` connection with original dependency
  direction, relation kind, occurrence count, and source evidence. Reading can
  follow a call backward to its caller; it does not imply execution order.
- Up to three source `ranges` per reading step, at most twelve lines each.
  Windows start at the seed declaration, selected relationship occurrences,
  compiler-resolved target definitions, or the first declaration as a fallback.
- `related_tests` with their recorded relationship and whether the file is
  selected. Full test-file identification uses indexed syntax roles and current
  module provenance. Mixed implementation/test files require a compiler
  relationship involving test code, with an actual test occurrence as evidence.
  These associations do not prove test execution or behavioral coverage.
- `boundary` relationships to files outside the selected set, including shared
  infrastructure, other review areas, scope exclusions, and budget omissions.
  `boundary_total` counts relationships and `omitted_files_total` counts distinct
  directly connected files outside the selection. It does not count every
  transitively reachable file or unresolved target.
- `unresolved_total`, the unresolved dependency records in selected files.

Boundary and related-test examples are capped at twenty each. Their totals and
`boundary_truncated` expose omissions; related tests have `related_tests_total`.
Use `dependencies`/`dependents` to explore additional connections. Boundary/test
references outside the selected set are context pointers, not extra selected
files or extra reading steps.

### Repository areas and coverage

`review-areas` automatically chooses seeds and partitions eligible code files
into disjoint primary review units. It starts with the most connected remaining
file (path order breaks ties), uses the same bounded selection as explicit
context, then repeats on unassigned files. Large connected components split into
linked areas; boundary entries include `other_area` IDs. Re-running explicit
context on an area's seed can select different neighbors because it is not
restricted by earlier primary assignments.

A file is classified as shared infrastructure when it has distinct non-module
consumers across at least three parent directories, at least twice as many
consumers as outgoing dependencies, and a consumer count of at least
`max(8, ceil(sqrt(graph_nodes)))`. The size-aware threshold and fan-in/fan-out
balance avoid treating every widely used implementation as shared infrastructure. Shared files are not
primary members of repository areas and remain visible through boundary
connections in multiple areas. Explicit context can still start at a shared
file. This rule is heuristic and is reported in the response notices.

Coverage accounts for every indexed supported code file within scope:

- `assigned_files`: files in primary areas, without overlap.
- `shared_files_total` / `shared_files`: shared infrastructure, separately listed.
- `unassigned_total` / `unassigned`: files that could not be assigned, with a
  reason. Initially, files with syntax parse errors are unassigned.
- `indexed_code_files` equals assigned + shared + unassigned counts.
  `excluded_non_code_files` separately counts indexed documents/resources.

Supported files without graph connections receive singleton areas. In
particular, a language without resolved dependencies can produce many singleton
areas. Full assignment accounting does not establish full semantic analysis;
inspect `analysis_coverage`, `index.languages`, and notices.

Areas and shared/unassigned coverage lists are paginated independently with the
same page offset. `--limit` accepts 1–20 (default 5). Continue `next_cursor` using
the same scope, file budget, and page limit until it is absent; later pages may
contain only coverage records. Cursors bind the repository, indexed source,
analysis state, and options. `areas_total` and coverage counts apply to the whole
query, not just the current page. Empty repositories return no areas.

Review areas are proposed work units, not confirmed subsystem boundaries,
automated findings, or risk rankings. The first version does not expand missing
imports, infer runtime paths, execute tests, or generate LLM descriptions.

The implementation keeps clustering language-neutral: `analysis/graph.rs` loads
and groups evidence once for all analysis workflows, while
`analysis/review/engine.rs` performs pure graph selection without database or
language-provider access. Rendering attaches source windows and test provenance
from the checked snapshot. Adding a language's dependency facts does not require
another clustering implementation.


## Change-driven review

```sh
sonar init
sonar impact --base main --max-files 12
# Explicitly attempt old compiler relationships as well:
sonar impact --base main --baseline-resolve --max-files 12
```

MCP exposes `sonar_impact` with `base`, `baseline_resolve`, `max_files`, `scope`,
`limit`, and `cursor`. Library callers use `impact` / `ImpactOptions`. Responses
now use `version: 2` and `policy: impact-v2`; Rust struct literals must supply
`baseline_resolve` or use `..Default::default()`.

`base` defaults to `HEAD`. Comparison is **base commit to current working-tree
contents**, including net staged/unstaged changes and nonignored untracked files.
This is not staged-only review or automatic merge-base comparison. The index must
be at the Git root and match current source. Git renames remain deletion/addition
records; declaration matching can propose a cross-file identity separately.

### Isolated baseline

Baseline syntax analysis is enabled by default. Sonar reads tracked blobs into a
temporary directory and uses the same index, language extractors, and navigation
infrastructure as current-source analysis. It does not switch branches, register
worktrees, run checkout filters/hooks, modify the Git index, or update the current
Sonar database. Symlinks and submodules are not followed; `baseline.excluded_entries`
and its total report these omissions. LFS filters and dependency installation are
not run by the snapshot loader.

`baseline_analysis` is true when this snapshot was analyzed.
`baseline.syntax_snapshot` identifies its indexed contents, and `parse_error_files`
reports baseline syntax failures. Baseline locations include their commit and
`snapshot: baseline`; their `decl:` IDs are comparison identities, **not current
navigation handles**. Read old source with Git at the reported commit and path.

Baseline snapshots and indexes are cached across requests and continuation pages.
Syntax entries are separate from compiler entries, so changing compiler settings
can reuse the baseline syntax index. Cold and warm requests return identical
review evidence; cache bookkeeping is available through the maintenance commands.

### Baseline cache

The default location is `$HOME/.sonar/baselines` (`USERPROFILE` when HOME is absent).
Set `SONAR_BASELINE_CACHE` to use another directory. Sonar requires an empty or
previously owned cache directory; if caching is unavailable, impact analysis falls
back to temporary preparation.

Syntax keys include the repository, commit, Sonar/extraction policy, resource
metadata, and language providers. Compiler keys additionally include recorded
resolver settings, executable contents and versions, and the process environment
(excluding navigation and cache-control variables). Environment values are hashed,
not stored in manifests. Reuse verifies source and index contents, recorded provider
freshness, and external Cargo dependency trees. TypeScript and Go also validate
their recorded compiler inputs. Changed, corrupt, or obsolete entries are rebuilt.

Entries become ready only after preparation and validation. Cross-process leases
protect publication, reads, and cleanup; failed compiler preparation is not retained
as a reusable compiler entry. Compiler output is built at a stable isolated path,
and the current repository and index are never used as cache build destinations.

The defaults are eight entries and 4 GiB of ready artifacts. Set positive
`SONAR_BASELINE_CACHE_MAX_ENTRIES` and `SONAR_BASELINE_CACHE_MAX_BYTES` to change them.
Least recently used entries are pruned after requests. Active entries are protected,
so active builds can temporarily exceed these limits; lock metadata and temporary
build overhead are additional to ready artifact sizes.

```sh
sonar baseline-cache list
sonar baseline-cache prune --max-entries 4 --max-bytes 2147483648
sonar baseline-cache prune --all
```

Listing reports ready, obsolete, and incomplete entries. Pruning skips entries
protected by active leases and reports remaining quota excess. These commands do
not require an initialized project.

Arbitrary external files read by build scripts, compiler wrappers, or plugins
cannot all be discovered automatically. For builds with such inputs, set
`SONAR_BASELINE_CACHE_COMPILER=0` to prepare compilers afresh while retaining syntax
caching, or prune the cache explicitly. Warm requests still validate inputs and
assemble review evidence; caching does not eliminate all repository scanning.

### Declaration matching

`declaration_changes` shows before/after coordinates and bounded textual signatures:

- A unique path/language/kind/symbol key is a structural identity match.
- Repeated names, such as overloads, can match through a unique textual signature
  within that key.
- Remaining declarations can become `possible_rename_or_move` candidates when
  source is unique after masking only the declaration's own name. Such candidates
  are always `uncertain: true`. Identical duplicated bodies are not forced into a
  match. Same-path/name fallback candidates are labeled `uncertain_match`.
- Unmatched syntax is labeled `added` or `removed`, with uncertain logical identity.
  These labels do not prove that rewritten or renamed code has no counterpart.
- Baseline/current parse failures produce explicit unavailable-side statuses.

`signature_changed` compares complete trimmed signature text; `source_changed`
compares complete declaration bytes. Formatting can change these flags. They do
not establish semantic equivalence, ABI compatibility, or a regression. Displayed
signatures are capped at 2,048 characters with `signature_truncated`; comparison
still uses the full signature.

Current `declarations` include changed-line candidates and reliable/candidate
counterparts discovered through baseline old-line ranges. `baseline_declarations`
map deleted/replaced old lines and matched counterparts. Pure deletions therefore
map to their original declaration rather than an adjacent surviving function.
Nested containers remain candidates when their source spans overlap an edit.
Attributes or comments outside indexed declaration ranges can remain file-level
changes rather than mapped declarations.

### Consumers, paths, and relationships

`consumers` are current compiler callers/references. With baseline compiler data,
`baseline_consumers` provides old caller coordinates, target comparison IDs, and
source occurrences at the baseline commit. Old evidence is never labeled with a
current `sn1:` handle. Empty lists do not prove absence of impact.

`cluster` uses the shared review engine and prioritizes reachable current consumer
files. `baseline_cluster` supplies old reading paths, including for removed code,
and prefers old consumers when available. It can use module relationships even
without a compiler. Both clusters have a hard `max_files` budget (1–50, default
12) **independently per changed file**; they can overlap with other change clusters.
Boundary and related-test limits still apply. Counterparts outside the selected
scope can appear as comparison metadata; selected reading files respect scope.

`relationship_changes` compares observed file-pair/kind occurrence counts and
reports `observed_added`, `observed_removed`, or `count_changed`. Compiler-derived
kinds are compared only when both snapshots have current analysis for the source
language. Missing baseline analysis is never treated as an empty graph proving
new calls. Source-layout module facts can be compared without compiler resolution.
These are aggregate observations, not complete execution-path differences.

### Optional baseline compiler preparation

`--baseline-resolve` / `baseline_resolve: true` explicitly attempts compiler work
inside isolated baseline storage. Ordinary unavailable-tool/dependency failures retain
syntax results and report `baseline.compilers` statuses and errors. Python remains
syntax-only. Existing resolver policies, build contexts, and analysis limitations
apply; `baseline.notices` exposes provider coverage and diagnostics summaries.

- Rust reuses recorded analyzer settings and executable (default `rust-analyzer`
  for older indexes). Cargo preparation uses the existing locked/offline resolver.
- TypeScript reuses the recorded Node/compiler selection and remaps project paths
  into the baseline. Run current `sonar resolve --typescript ...` with the updated
  executable to record these settings. External project configurations are not
  silently treated as baseline files.
- Go reuses recorded executable, tags, and target options, or existing defaults.

Snapshots contain tracked regular files, not ignored dependency directories or
Git metadata. Builds needing absent local dependencies, untracked generated files,
submodules, or Git-derived build information may fail or have reduced coverage.
A prepared compiler can still report diagnostics or omit targets. If preparation
changes baseline source, freshness checks reject the report rather than using
modified evidence. No current workspace compiler/index is refreshed implicitly.

### Paging and limits

`limit` accepts 1–20 changed files per page (default 5). Continue `next_cursor` with
identical base, scope, file budget, page limit, and baseline resolution mode.
Cursors bind source/Git state, matching observations, compiler availability, and
normalized baseline relationships. Temporary paths and SQLite IDs do not change
continuation identity. Git and current source are rechecked before returning.

Each side returns up to 100 declaration candidates and the report returns up to
100 declaration comparisons, 100 hunks, 20 consumers per side, and 20 file
relationship changes per file. Full totals and `details_truncated` identify omitted
records. Selection uses all mapped candidates before display caps. `truncated`
separately indicates remaining change pages. Use current handles with `neighbors`
/ `follow`; baseline evidence must be read at its baseline commit.

The upgraded Rust import-leaf provider still requires an explicit `sonar init` or
`sonar index .` after upgrading older dependency indexes. Existing current compiler
evidence can be reused when its sources and build inputs are unchanged.


## Review packs

`review-pack` assembles source and static evidence for an LLM or human reviewer.
It does not invoke a model or generate findings. Audit mode reuses review areas;
change mode reuses impact analysis, including its baseline cache and matching
limitations. The shared pack builder has no language-specific grouping rules.

```sh
# Automatically partition code into audit packs.
sonar review-pack --scope src --max-files 8 --max-tokens 16000 --limit 3

# Focus one pack on a file, exact symbol, or current navigation handle.
sonar review-pack --seed src/service.rs --max-files 6

# Review working-tree changes against a base commit.
sonar review-pack --base HEAD --max-files 8
sonar review-pack --base main --baseline-resolve --max-tokens 24000
```

`--base` selects change review and conflicts with `--seed`. With neither option,
Sonar proposes automatic audit areas. `--baseline-resolve` requires `--base` and
explicitly attempts the existing baseline compiler preparation. This MVP is
available through the CLI and Rust API (`analysis::review_pack`); it adds no MCP tool.

Each pack contains a deterministic identity and query/state fingerprint, current
snapshot and optional baseline commit, a reading path with exact source excerpts,
evidence-backed connections, boundary relationships, and related tests. Change
packs also include bounded diff hunk coordinates, before/after declaration
signatures and matching uncertainty, relationship changes, and analysis gaps.
Baseline source is read from the reported Git commit; its paths and ranges must
not be interpreted as current navigation handles.

`max_files` (1–50) counts source versions: current and baseline versions of one
path consume two slots. Change packs prioritize the changed file in both snapshots
before expanding into consumers. A metadata budget may select fewer files.
Reading steps retain their original parent and connection evidence even when the
parent cannot fit; these references do not imply its source is present in the pack.

`max_tokens` (2048–65536, default 16000) uses the explicit
`compact-json-utf8-bytes-v1` policy: **one compact serialized JSON UTF-8 byte is one
conservative token unit**, including metadata, escaping, and the count field itself.
This is a model-independent sizing policy, not an exact model tokenizer count.
`estimated_tokens` equals the compact byte size and never exceeds the budget.
Pretty-print whitespace, the response envelope, and any downstream prompt are
additional; tokenize the final prompt with the target model when exact accounting
is required. The budget applies separately to each pack; `--limit` bounds packs
per page (1–20).

Metadata admission reserves at least a quarter of the budget for source. Source
lines are admitted in reading order, round-robin across selected files, with at
most 120 requested lines per file. Oversized lines can be omitted whole; admitted
excerpts retain exact text, original line endings, and one-based inclusive ranges.
No summaries or invented replacement source are inserted.

Pack `totals`, per-file requested/included line counts, and `truncated` expose
omissions. Totals describe candidate source versions, outside files, boundaries,
tests, unresolved records, hunks, declaration/relationship changes, gaps, and
unavailable sources. They do not imply complete semantic coverage. The response
envelope carries provider coverage/notices, optional baseline preparation details,
and complete audit assignment accounting through the existing coverage pages.
Retain that envelope when handing packs to a reviewer; zero recorded connections
is not proof of isolation or safety.

Use `next_cursor` with exactly the same options to continue automatic audit or
change packs. Seed packs have no continuation. Cursors bind the pack budget and
selection options as well as the upstream snapshot and analysis state; stale or
mismatched continuations are rejected. Response `truncated` describes upstream
paging; individual pack `truncated` additionally describes payload omissions.
Pack IDs change with options, indexed sources, dependency analysis, or Git diff
state. Regenerate a pack to check its identity before acting on a saved review.
Source text is untrusted review data, not instructions for an executing agent.
