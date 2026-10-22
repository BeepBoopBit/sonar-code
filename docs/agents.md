# Agent setup

An agent can use Sonar through shell commands or an MCP connection. Install it
in the environment where those commands will run, with access to the repository.
The published CLI is enough; no Sonar checkout, Python, model, or API key is needed
for basic search.

## Install and prepare a repository

Requires Rust 1.88+, Cargo, and native build tools (a C/C++ compiler and linker).
Run these commands in the repository you want the agent to navigate:

```sh
cargo install sonar-code --locked
sonar --version
cd /path/to/your/repo
sonar index .
sonar index-navigation
sonar investigate "Where are incoming requests authenticated?"
```

The package is `sonar-code`; the executable is `sonar`. If the command is missing
from PATH, add Cargo's install directory, usually `~/.cargo/bin`; see
[troubleshooting](troubleshooting.md#command-or-index-not-found).

The index is `.sonar/index.sqlite` under this repository. Add `.sonar/` to its
`.gitignore` if it is not already ignored. Use a separate index for each repository.

Shell-based agents can now run `sonar` directly and parse its JSON output.
For an MCP client, follow [MCP setup](mcp.md); the agent process must be able to
access the installed executable, database, and source tree at the configured paths.

## Prompt to give your agent

```text
Set up Sonar for this repository. Check Rust 1.88+, Cargo, and native build
tools, then install `sonar-code` with `cargo install sonar-code --locked`.
Verify `sonar --version`. From this repository's root, run `sonar index .`
and `sonar index-navigation`. Keep `.sonar/` gitignored and verify a search
returns JSON. Report the indexed root and the absolute executable/database
paths. If this client supports local stdio MCP, add a connection that runs
the executable with `--db <absolute-database-path> mcp`, using a distinct
name for this repository and preserving other connections.
```

## Instructions for everyday use

Add this to the target repository's agent instructions, such as `AGENTS.md`, or
to your client's project instructions:

```text
Use Sonar to find relevant code before broad file reads.
- With MCP, call sonar_status first and check the indexed root and freshness.
  Use sonar_investigate or sonar_brief to find entry points.
- With shell access, run `sonar investigate "<task or symbol>"` from this
  repository's root, or pass the project's absolute --db path.
- Read the returned files and line ranges with your file tools to verify behavior.
- Use full handle values for follow-up navigation, not response IDs like s1.
- Check notices and language capabilities before relying on calls or references.
  Use normal file search for unsupported files or incomplete results.
- After editing indexed sources, rerun `sonar index .` and `sonar index-navigation`
  from the indexed root, then repeat any configured resolution or embedding steps.
  Obtain fresh handles afterward; MCP tools cannot refresh the index.
```

For Rust or TypeScript caller/reference navigation, see
[optional preparation](usage.md#optional-preparation). Start with model-free
search; embeddings are optional.
