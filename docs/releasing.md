# Releasing

The crates.io package is [**`sonar-code`**](https://crates.io/crates/sonar-code);
its executable and Rust library are named **`sonar`**.
Cargo builds from source, so users need Rust 1.88+ and native build tools.

Publishing runs only through [GitHub Actions](../.github/workflows/publish.yml)
when a version tag such as `v0.1.0` is pushed. Branch pushes do not publish.

## Release setup

- Release from the [GitHub repository](https://github.com/BeepBoopBit/sonar-code).
- Use a verified crates.io account that owns `sonar-code` and an API token with
  permission to publish new versions of the crate.
- Add that token as the repository's `CARGO_REGISTRY_TOKEN` Actions secret under
  **Settings → Secrets and variables → Actions**. No local `cargo login` is needed.

Linux is the locally validated platform. Check builds and tests on macOS and
Windows before advertising support for those platforms.

## Validate

From the checkout:

```sh
./dev.sh test
./dev.sh check
cargo +1.88.0 check --locked --all-targets
cargo package --list --locked
cargo publish --dry-run --locked
```

Install Rust 1.88.0 with `rustup toolchain install 1.88.0 --profile minimal` if
needed. While preparing uncommitted changes, add `--allow-dirty` to the two
packaging commands. Use a committed, clean checkout for the actual release.

Inspect the package list: it must contain `LICENSE`, `Cargo.lock`, and
`src/languages/typescript/compiler.cjs`, and exclude local indexes, models,
evaluation data, and build output. The dry run packages and builds the source
without uploading it.

Test installation from that extracted package into a temporary directory.
Replace `0.1.0` below with the version being released:

```sh
install_root="$(mktemp -d)"
cargo install --path target/package/sonar-code-0.1.0 --locked --root "$install_root"
"$install_root/bin/sonar" --version
"$install_root/bin/sonar" --help
```

## Publish

Update `Cargo.toml` and `Cargo.lock`, then commit and push the release files.
Push a tag matching the new version; for example, after setting version `0.1.1`:

```sh
git push origin main
git tag -a v0.1.1 -m "Release 0.1.1"
git push origin v0.1.1
```

The workflow checks the tag, formatting, Clippy, Rust and helper tests, Rust 1.88
compatibility, and a publishing dry run. Publication starts only after all checks
pass. Follow its progress in the repository's **Actions** tab. Do not run a real
`cargo publish` locally.

Published versions cannot be overwritten. Each release needs a new version and
tag. If a run fails because the secret is missing, add it and rerun the failed
workflow jobs.

The workflow uses an API token and can be migrated
to [crates.io Trusted Publishing](https://crates.io/docs/trusted-publishing).

Users install the published CLI with:

```sh
cargo install sonar-code --locked
sonar --version
```

Cargo installs the executable only. `dev.sh` and optional Python helpers remain
checkout tools; ordinary indexing, querying, and MCP serving use `sonar` directly.

See the [Cargo publishing guide](https://doc.rust-lang.org/cargo/reference/publishing.html).
