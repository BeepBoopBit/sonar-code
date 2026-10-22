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
sonar init
sonar status
sonar investigate "Where are incoming requests authenticated?"
```

The package is `sonar-code`; the executable is `sonar`. If the command is missing
from PATH, add Cargo's install directory, usually `~/.cargo/bin`; see
[troubleshooting](troubleshooting.md#command-or-index-not-found).

The index is `.sonar/index.sqlite` under this repository. Initialization manages
its `.gitignore` rule and saves paths and preparation settings in
`.sonar/project.json`. Commands from the project or its subdirectories reuse them.
Check that `status` reports `initialized: true`, the intended absolute `root`,
and `index.syntax_freshness: "current"`. Use a separate index for each repository.

Shell-based agents can now run `sonar` directly and parse its JSON output.
For an MCP client, run `sonar setup codex` or `sonar setup claude`. These commands
also initialize the project, write client configuration, and maintain usage
instructions while preserving existing content. Reload the client and accept
project trust if requested. See [MCP setup](mcp.md) for manual configuration;
the client needs access to the executable, database, and source tree.

## Prompt to give your agent

```text
Set up Sonar for this repository. Check Rust 1.88+, Cargo, and native build
tools, then install `sonar-code` with `cargo install sonar-code --locked`.
Verify `sonar --version`. For this client's MCP connection, run
`sonar setup codex` or `sonar setup claude` from the repository root.
Reload the client configuration and verify sonar_status and a search.
For shell-only use, run `sonar init`, then `sonar status` and a search.
Report the indexed root and absolute executable/database paths.
Manual MCP configuration is documented at:
https://github.com/BeepBoopBit/sonar-code/blob/main/docs/mcp.md
```

## Instructions for everyday use

Native setup maintains a usage block in `AGENTS.md` or `CLAUDE.md`. For a manual
connection, add instructions such as these to the project's existing instructions:

```text
Use Sonar to find relevant code before broad file reads.
- With MCP, call sonar_status first and check the indexed root and freshness.
  If initialized is false, call sonar_init with {}, then check status again.
  Use sonar_investigate or sonar_brief to find entry points.
- With shell access, run `sonar investigate "<task or symbol>"` from this
  repository's root, or pass the project's absolute --db path.
- Read the returned files and line ranges with your file tools to verify behavior.
- Use full handle values for follow-up navigation, not response IDs like s1.
- Check notices and language capabilities before relying on calls or references.
  Use normal file search for unsupported files or incomplete results.
- After editing indexed sources, rerun `sonar init` from the indexed root to
  refresh saved preparation. MCP clients can call the explicit sonar_init tool.
  Obtain fresh handles afterward.
```

For Rust or TypeScript caller/reference navigation, see
[optional preparation](usage.md#optional-preparation). Start with model-free
search; embeddings are optional.

If `sonar_status` reports `initialized: false`, call `sonar_init` with `{}`. It
initializes the fixed repository root configured for this server, updates the
ignore rule, and saves settings. Search/navigation tools remain read-only and
require initialization. `sonar_init` can also refresh saved preparation steps.
