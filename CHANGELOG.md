# Changelog

## 0.2.0 (2026-11-07)

Sonar now prepares dependency-backed review groups, change-impact reports, and
bounded source packs for code reviewers and coding agents. These features provide
review context; improved LLM bug detection has not yet been established.

### Upgrade notes

- Refresh existing indexes with `sonar init` or `sonar index .` to prepare the
  new dependency facts. Repeat optional compiler resolution when needed.
- Dependency and impact responses use version 2. Rust library users should
  review the new analysis APIs and response types when upgrading.
- Review packs are available through the CLI and Rust API. Dependencies, review
  context, review areas, and impact analysis are also exposed through MCP.

### Python import context

- Add indexed Python module/package import leads through the existing dependency
  provider interface, including aliases, `src/` layouts, and regular-package
  relative imports. Preserve ambiguous, external/missing, namespace, stub-only,
  wildcard, and recognizable dynamic imports as explicit gaps.
- Include linked policy/settings files in review packs and retain header contracts
  through shared module-level range selection (`review-clusters-v2`). Bind review
  identities/continuations to provider coverage and require old indexes to refresh.
- Recheck the review corpus for delivery of both previously missing Python
  supporting spans; this measures context availability, not LLM detection quality.

### Dependency analysis

- Add file dependencies and reverse dependents through CLI and MCP, reusing
  current compiler relationships and Rust syntax module facts.
- Group relationships by file pair, with counts and up to three evidence
  examples per kind. Dependency responses use version 2.
- Paginate groups, unresolved records, and skipped-file paths using continuations
  bound to the query, source generation, and analysis state. Page size no longer
  limits graph exploration. See [dependency analysis](docs/analysis.md).

### Review clusters

- Build bounded review context around a file, exact symbol, or navigation handle,
  or propose repository-wide review areas through CLI and MCP.
- Share deterministic graph selection across both workflows, with hard file
  budgets, evidence-backed reading paths, bounded source ranges, related test
  relationships, and explicit boundary omissions.
- Keep primary areas disjoint, identify high-degree shared infrastructure, and
  report complete assigned/shared/unassigned code-file accounting. Paginate
  areas and coverage with analysis-bound continuations. These are static work
  units, not execution traces or defect rankings.

### Review evaluation

- Add a dependency-free experiment harness comparing review packs, diffs, and
  full files on executable regression fixtures and correct controls.
- Validate findings against exact delivered source and snapshot IDs; require
  explicit semantic adjudication before reporting precision and recall.
- Track reviewer time, input bytes, optional reported token usage, and annotated
  context gaps. Keep failed/missing trials and unjudged findings explicit.

### Review packs

- Add `sonar review-pack` and a shared Rust API for automatic audit areas, seeded
  review context, and change review against a Git base.
- Bundle exact current/baseline source excerpts, reading order, connections,
  before/after declarations, boundaries, related tests, and explicit omission counts.
- Bound each compact JSON pack with a conservative byte-based token policy;
  bind identities and paginated continuations to options, sources, and analysis state.

### Baseline cache

- Reuse isolated baseline syntax and compiler indexes across impact requests,
  validating source contents, resolver inputs, tool versions, and provider freshness.
- Publish entries under cross-process leases, retain syntax independently of compiler
  settings, and bound retained artifacts with automatic least recently used cleanup.
- Add `sonar baseline-cache list` and `prune`, configurable storage/limits, and an
  option to bypass compiler reuse for builds with opaque external inputs.

### Change-driven review

- Resolve individual Rust import/re-export leaves through exact current compiler
  bindings, including aliases and grouped imports. Preserve external, missing,
  ambiguous, and glob targets as unresolved. Refresh older syntax indexes.
- Add `sonar impact` and `sonar_impact`: compare a Git base with the current
  working tree, map current line-overlap declaration candidates, find recorded
  consumers, and build bounded review clusters preferring those consumers.
- Report deletions and missing baseline analysis, non-code changes, detail caps,
  and analysis gaps. Paginate with diff/snapshot-bound continuations and recheck
  Git/source state before returning results.

### Baseline-aware impact

- Upgrade impact responses to version 2 with isolated baseline syntax snapshots,
  deleted-line declaration mapping, textual signature/source comparisons, and
  explicitly uncertain rename/move candidates. Repeated names can match through
  unique signatures; ambiguous duplicated bodies remain unmatched.
- Add opt-in `--baseline-resolve` / MCP `baseline_resolve` to reuse Rust,
  TypeScript, and Go resolver settings for old consumers. Failures and missing
  providers remain explicit while syntax comparison stays available.
- Return baseline-coordinate consumers and bounded old reading clusters, plus
  comparable file-relationship deltas. Baseline IDs are not current navigation
  handles. Temporary snapshots do not switch/register worktrees or change the
  current index; validated baseline indexes can be reused through the cache.

## 0.1.2

### Go navigation

- Index `.go` declarations, receiver-qualified methods, structs, interfaces,
  fields, aliases, generics, package variables/constants, and documentation.
  Declarations in `_test.go` files have the test source role. Syntax indexing
  works without a Go installation.
- Prepare calls, references, field uses, and interface implementations with
  `sonar init --go` or `sonar resolve --go`. Analysis uses a compatible local Go
  toolchain and locally available dependencies, with module/toolchain downloads
  disabled. Go 1.18 is the analyzer's minimum supported toolchain.
- Save build tags, target OS/architecture, and a custom Go executable in the
  initialization profile. Analyze module, workspace, nested-module, vendor,
  and test-package build contexts.
- Track named field uses and positional struct initializers, preserving
  references inside initializer values. Resolve calls through local function
  aliases with a single initializer and no reassignment or address-taking.
- Find interface implementations through `neighbors` and `follow` using
  `--relation implements`, including method links, embedded methods, aliases,
  and observed generic instantiations. CLI and MCP expose the same options.
- Keep queries on saved evidence and invalidate analysis after relevant source,
  dependency, build configuration, environment, or toolchain changes.

### Continuous integration

- Run shared checks on branch pushes and pull requests, and reuse them before
  publishing a release tag.
- Require real Rust, TypeScript, and Go compiler integrations in CI. Test
  TypeScript 5.9.3/6.0.3, Go 1.18.10 compatibility, and Rust 1.88 compilation,
  alongside all structural, document/resource, CLI, MCP, and helper tests.
- Fail on unavailable explicitly configured test compilers, and make missing
  compiler tools fail when `SONAR_REQUIRE_COMPILERS=1` is set.

### Compatibility and upgrade

This early-stage release intentionally includes Rust library API changes:

- `Relation` and `FollowRelation` gain an `Implements` variant. Exhaustive
  matches must handle it.
- `onboarding::Configuration` gains `go: Option<GoOptions>`. Existing struct
  literals must add `go: None` or configure Go; exhaustive destructuring must
  include the field or `..`. Older saved project settings still load.
- Custom JSON consumers should accept the new `implements` relationship kind.

After installing the updated executable, run `sonar init` to refresh existing
indexes, or `sonar init --go` to also prepare Go compiler analysis. Obtain fresh
navigation handles after reindexing and restart running MCP clients after
replacing the executable.

### Analysis limits

Interface calls target interface declarations; runtime dispatch is not inferred.
Mutable callbacks, callback parameters, function-valued globals/fields, returned
functions, and anonymous function targets remain unresolved. Implementation
links compare indexed declarations within each package load, using value or
pointer method sets and observed generic instances. Empty interfaces, type-set
constraints, and package loads with type errors produce no implementation links.

Linux is the validated platform for this release.
