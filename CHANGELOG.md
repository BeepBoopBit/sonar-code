# Changelog

## 0.1.2 (unreleased)

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
