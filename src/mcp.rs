//! Project initialization and code navigation tools over MCP.
use std::{path::Path, time::Duration};

use anyhow::{Context, Result};
use rmcp::{
    ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Implementation, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
};

mod params;
mod runtime;

use runtime::{Operation, Worker};

const INSTRUCTIONS: &str = "Check sonar_status first. If initialized=false, call sonar_init to explicitly prepare this server's bound repository. Search/navigation require initialization. The init tool writes local project state and ignore rules; query tools only read. Sonar navigates the one repository bound to this server's index. Start with sonar_status for its root, freshness and index.languages capabilities (Rust and TypeScript/TSX support compiler-resolved calls, references and field uses after explicit resolution; Python supports structural navigation; contextual facts currently require Rust), sonar_brief for compact entry points, or sonar_investigate for separate function/documentation leads. Read the returned source ranges using your file tools; Sonar complements grep and does not answer behavior questions for you. Pass a location's full handle (not s1/n1 response IDs) to investigate.from, neighbors, follow, or field_uses. Source roles, routes and overlap are evidence to inspect, not execution-flow, test-coverage or confidence guarantees. Empty results never establish feature absence. Paths are relative to the root reported by status. Use sonar_init to refresh saved preparation settings, or use explicit local CLI indexing, embedding, and resolution operations. The optional configured model is reused across calls. Current snapshots are required for handle navigation. Follow and neighbors have their own scope; field_uses scope restricts use sites, not referenced declarations. Cancellation skips queued work; an active synchronous read can finish internally without sending its cancelled result.";

#[derive(Clone)]
struct SonarServer {
    worker: Worker,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl SonarServer {
    fn new(worker: Worker) -> Self {
        Self {
            worker,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Explicitly initialize or refresh this server's bound repository: index source files, prepare basic navigation, save project settings, and add .sonar/ to .gitignore. Reuses saved optional compiler/model preparation. Does not modify AI client registrations.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sonar_init(&self, Parameters(_): Parameters<params::Empty>) -> CallToolResult {
        self.worker.call(Operation::Init).await
    }

    #[tool(
        description = "Inspect the configured repository root, index freshness, per-language capabilities, analysis availability and retrieval setup. Use this first when the repository or preparation state is unclear.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sonar_status(&self, Parameters(_): Parameters<params::Empty>) -> CallToolResult {
        self.worker.call(Operation::Status).await
    }

    #[tool(
        description = "Find compact starting locations for a task or exact symbol/path. Returns ranked paths, handles, bounded reading ranges and supporting links; no generated answer. Use role=implementation with kind=function to find code owners across all indexed languages when documentation or tests dominate. Role and kind constrain starts; supporting locations retain their own roles. Read the suggested source to verify behavior.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sonar_brief(&self, Parameters(p): Parameters<params::Brief>) -> CallToolResult {
        self.worker.call(Operation::Brief(p)).await
    }

    #[tool(
        description = "Investigate a task with five primary leads plus separate implementation-function/documentation leads (up to eleven unique locations). Supply from with a full location handle to instead inspect evidenced callers, callees, references, test code and literal document links. Search leads are not confirmed owners or contracts; use the returned focus and overlap to choose what to read.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sonar_investigate(
        &self,
        Parameters(p): Parameters<params::Investigate>,
    ) -> CallToolResult {
        self.worker.call(Operation::Investigate(p)).await
    }

    #[tool(
        description = "Turn a repository-relative source path and line into the smallest indexed declaration and a snapshot-bound navigation handle. Use after a file read or grep hit.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sonar_locate(&self, Parameters(p): Parameters<params::Locate>) -> CallToolResult {
        self.worker.call(Operation::Locate(p)).await
    }

    #[tool(
        description = "List a file's indexed declarations or Markdown sections in source order, with handles and parent relationships. Continue using next_cursor; no model required.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sonar_outline(&self, Parameters(p): Parameters<params::Outline>) -> CallToolResult {
        self.worker.call(Operation::Outline(p)).await
    }

    #[tool(
        description = "Inspect a handle's recorded containment, calls, references or literal document/resource links, with source evidence and pagination. Scope constrains both endpoints. Missing analysis is reported; static links do not prove runtime behavior.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sonar_neighbors(
        &self,
        Parameters(p): Parameters<params::Neighbors>,
    ) -> CallToolResult {
        self.worker.call(Operation::Neighbors(p)).await
    }

    #[tool(
        description = "Search a known handle's bounded call/reference neighborhood for a behavior and return an evidenced route to each match. Requires current compiler analysis. Scope applies to the entire route; direction applies at every hop. Inspect explicit traversal limits.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sonar_follow(&self, Parameters(p): Parameters<params::Follow>) -> CallToolResult {
        self.worker.call(Operation::Follow(p)).await
    }

    #[tool(
        description = "Inspect compiler-resolved field occurrences and bounded source associations from a field or function handle. Scope filters use-site files; referenced declarations may be outside it. Access labels describe syntax, not general data flow. Paginate with next_cursor.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn sonar_field_uses(
        &self,
        Parameters(p): Parameters<params::FieldUses>,
    ) -> CallToolResult {
        self.worker.call(Operation::FieldUses(p)).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for SonarServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("sonar", env!("CARGO_PKG_VERSION")))
            .with_instructions(INSTRUCTIONS)
    }
}

/// Serve one prepared index over stdio until the client closes the transport.
/// Standard output is reserved for MCP frames; setup failures return to the CLI's stderr.
/// The optional model is reused. Refresh explicitly through sonar_init or the CLI.
pub fn serve_mcp(db: &Path, model: Option<&Path>) -> Result<()> {
    serve_mcp_project(db, model, None)
}

pub fn project_status(
    db: &Path,
    model: Option<&Path>,
    root: Option<&Path>,
) -> Result<serde_json::Value> {
    runtime::project_status(db, model, root)
}

pub fn serve_mcp_project(db: &Path, model: Option<&Path>, root: Option<&Path>) -> Result<()> {
    let (worker, thread) = Worker::start(db, model, root)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let result = runtime.block_on(async move {
        let service = SonarServer::new(worker)
            .serve(rmcp::transport::stdio())
            .await?;
        service.waiting().await?;
        Ok::<_, anyhow::Error>(())
    });
    // Tokio's stdin reader can still be blocked if the peer failed its handshake
    // without closing stdin. Do not let runtime teardown hang this CLI invocation.
    runtime.shutdown_timeout(Duration::from_millis(100));
    thread
        .join()
        .map_err(|_| anyhow::anyhow!("navigation worker panicked"))?;
    result.context("MCP stdio service stopped")
}
