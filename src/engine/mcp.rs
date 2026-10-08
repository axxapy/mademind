//! MCP handler for the builtin engine: rqmd-mcp's `QmdMcpServer` with live
//! `instructions`.
//!
//! rqmd-mcp builds the instructions (document count, collections, "no
//! embeddings yet") once, when the server is created — before mademind has
//! indexed or embedded anything — and serves that text for the life of the
//! process. This wrapper passes every request through and only swaps the
//! instructions for a header built from the latest index status, which the
//! indexer thread refreshes after each update and embed. The static rest of
//! rqmd's text (search syntax, examples, retrieval, tips) is kept as is.

use std::sync::{Arc, RwLock};

use rmcp::model::{
    CallToolRequestParams, CallToolResult, ListResourceTemplatesResult, ListToolsResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResult, ServerInfo,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler};
use rqmd_core::RqmdStore;
use rqmd_mcp::QmdMcpServer;

/// Latest instructions, shared between the indexer (writes) and every MCP
/// session (reads).
#[derive(Clone)]
pub struct Instructions {
    text: Arc<RwLock<Option<String>>>,
    /// rqmd's static part, from "Search:" on.
    tail: Option<String>,
}

impl Instructions {
    pub fn new(server: &QmdMcpServer) -> Self {
        let tail = server
            .get_info()
            .instructions
            .and_then(|s| s.find("Search:").map(|i| s[i..].to_string()));
        Instructions {
            text: Arc::new(RwLock::new(None)),
            tail,
        }
    }

    /// Rebuild from the store's current status (indexer thread).
    pub fn refresh(&self, store: &RqmdStore) {
        let Ok(status) = store.status() else { return };
        let context = store.get_global_context().ok().flatten();
        let names: Vec<&str> = status.collections.iter().map(|c| c.name.as_str()).collect();
        let text = render(
            status.total_documents,
            context.as_deref(),
            &names,
            status.has_vector_index,
            status.needs_embedding,
            self.tail.as_deref(),
        );
        if let Ok(mut t) = self.text.write() {
            *t = Some(text);
        }
    }

    fn current(&self) -> Option<String> {
        self.text.read().ok().and_then(|t| t.clone())
    }
}

fn render(
    docs: i64,
    context: Option<&str>,
    collections: &[&str],
    has_vectors: bool,
    needs_embedding: i64,
    tail: Option<&str>,
) -> String {
    let mut lines = vec![format!(
        "mademind: search engine over {docs} markdown documents."
    )];
    if let Some(ctx) = context {
        lines.push(format!("Context: {ctx}"));
    }
    if !collections.is_empty() {
        lines.push(String::new());
        lines.push(format!(
            "Collections (scope with `collection` parameter): {}",
            collections.join(", ")
        ));
        lines.push(
            "Call the `status` tool for collection descriptions, paths, and per-collection doc counts."
                .into(),
        );
    }
    // Embedding runs on its own schedule inside the server; nothing for the
    // agent to run.
    if !has_vectors {
        lines.push(String::new());
        lines.push(
            "Note: semantic search (vec/hyde) is not ready yet; embeddings are being built in the background. Use lex until then."
                .into(),
        );
    } else if needs_embedding > 0 {
        lines.push(String::new());
        lines.push(format!(
            "Note: {needs_embedding} documents are not embedded yet (that runs in the background); vec/hyde may miss them until then."
        ));
    }
    if let Some(t) = tail {
        lines.push(String::new());
        lines.push(t.to_string());
    }
    lines.join("\n")
}

/// `QmdMcpServer` with `get_info` answering from [`Instructions`].
#[derive(Clone)]
pub struct McpServer {
    inner: QmdMcpServer,
    instructions: Instructions,
}

impl McpServer {
    pub fn new(inner: QmdMcpServer, instructions: Instructions) -> Self {
        McpServer {
            inner,
            instructions,
        }
    }
}

impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = self.inner.get_info();
        if let Some(text) = self.instructions.current() {
            info.instructions = Some(text);
        }
        info
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        self.inner.list_tools(request, context).await
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        self.inner.call_tool(request, context).await
    }

    async fn list_resource_templates(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        self.inner.list_resource_templates(request, context).await
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResult, McpError> {
        self.inner.read_resource(request, context).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_reports_live_counts_and_keeps_the_tail() {
        let t = render(
            256,
            Some("notes"),
            &["a", "b"],
            true,
            0,
            Some("Search: ..."),
        );
        assert!(t.starts_with("mademind: search engine over 256 markdown documents."));
        assert!(t.contains("Context: notes"));
        assert!(t.contains("Collections (scope with `collection` parameter): a, b"));
        assert!(!t.contains("Note:"));
        assert!(t.ends_with("Search: ..."));
    }

    #[test]
    fn render_never_tells_the_agent_to_run_embed() {
        let none = render(0, None, &[], false, 0, None);
        assert!(none.contains("not ready yet"));
        let some = render(10, None, &[], true, 3, None);
        assert!(some.contains("3 documents are not embedded yet"));
        assert!(!none.contains("rqmd embed") && !some.contains("rqmd embed"));
    }
}
