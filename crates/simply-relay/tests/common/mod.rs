//! A scripted stand-in for Lumina's listener, and a small app handler.

#![allow(dead_code)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, ListToolsResult,
    PaginatedRequestParams, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{NotificationContext, RequestContext, RoleClient, RoleServer, RunningService};
use rmcp::{ClientHandler, ErrorData, ServerHandler, ServiceExt};
use serde_json::{json, Value};
use simply_relay::frames::{json_frames, FrameSink, FrameStream};
use simply_relay::wire::{AppFrame, LuminaFrame, Refused, RefusedReason, Welcome};
use simply_relay::{Backoff, Builder, Relay};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;

pub const WAIT: Duration = Duration::from_secs(5);

// ── the app's handler ───────────────────────────────────────────────────

/// Four tools: `levels` (no argument), `echo` (one required argument),
/// `open_session` (an action) and `wipe`, which no test declares. Every call
/// that reaches it is recorded.
#[derive(Clone, Default)]
pub struct Studio {
    pub reached: Arc<Mutex<Vec<String>>>,
}

impl Studio {
    pub fn reached(&self) -> Vec<String> {
        self.reached.lock().unwrap().clone()
    }
}

fn schema(v: Value) -> Arc<serde_json::Map<String, Value>> {
    Arc::new(v.as_object().unwrap().clone())
}

impl ServerHandler for Studio {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().enable_prompts().build())
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let none = || schema(json!({"type": "object", "properties": {}}));
        Ok(ListToolsResult::with_all_items(vec![
            Tool::new("levels", "Levels.", none()),
            Tool::new(
                "echo",
                "Echo.",
                schema(
                    json!({"type": "object", "properties": {"text": {"type": "string"}}, "required": ["text"]}),
                ),
            ),
            Tool::new("open_session", "Open a session.", none()),
            Tool::new("wipe", "Wipe everything.", none()),
        ]))
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.reached.lock().unwrap().push(request.name.to_string());
        let result = match request.name.as_ref() {
            "levels" => CallToolResult::structured(json!({"rate": 48000, "peaks": [-6.0]})),
            "echo" => CallToolResult::success(vec![ContentBlock::text(
                request
                    .arguments
                    .and_then(|a| a.get("text").cloned())
                    .unwrap_or_default()
                    .to_string(),
            )]),
            other => CallToolResult::success(vec![ContentBlock::text(format!("{other} done"))]),
        };
        Ok(result.into())
    }
}

/// A relay for "studio" in `dir`, dialling `lumina`, with fast pacing.
pub fn relay(dir: &std::path::Path, lumina: &FakeLumina) -> Builder {
    Relay::builder("studio", "1.2.3", dir)
        .endpoint(lumina.url.clone())
        .label("test-mac")
        .read_only(["levels", "echo"])
        .action("session", ["open_session"])
        .backoff(Backoff { min: Duration::from_millis(20), max: Duration::from_millis(200) })
        .handshake_timeout(Duration::from_secs(2))
}

// ── Lumina's side ───────────────────────────────────────────────────────

pub struct FakeLumina {
    listener: TcpListener,
    pub url: String,
}

/// An accepted socket after the app's first frame.
pub struct Conn {
    sink: FrameSink<TcpStream, LuminaFrame>,
    stream: FrameStream<TcpStream, AppFrame>,
}

impl FakeLumina {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("ws://{}/devices/v1", listener.local_addr().unwrap());
        Self { listener, url }
    }

    /// The next device's first frame.
    pub async fn accept(&self) -> (AppFrame, Conn) {
        let (tcp, _) = tokio::time::timeout(WAIT, self.listener.accept())
            .await
            .expect("a device dials within the wait")
            .unwrap();
        let ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
        let (sink, mut stream) = json_frames::<_, LuminaFrame, AppFrame>(ws, None);
        let first =
            tokio::time::timeout(WAIT, stream.next()).await.unwrap().expect("a first frame");
        (first, Conn { sink, stream })
    }
}

/// What a connected MCP client saw.
pub struct Mcp {
    pub client: RunningService<RoleClient, Watcher>,
    pub list_changed: mpsc::UnboundedReceiver<()>,
}

pub struct Watcher(mpsc::UnboundedSender<()>);

impl ClientHandler for Watcher {
    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        let _ = self.0.send(());
    }
}

impl Conn {
    pub async fn refuse(mut self, reason: RefusedReason, message: &str) {
        let refused = LuminaFrame::Refused(Refused { reason, message: message.into() });
        self.sink.send(refused).await.unwrap();
        let _ = self.sink.close().await;
    }

    /// Welcome the device and connect to it as Lumina's MCP client.
    pub async fn welcome(mut self, device_id: &str, token: &str, member: Option<&str>) -> Mcp {
        let welcome = Welcome {
            device_id: device_id.into(),
            token: token.into(),
            member: member.map(Into::into),
        };
        self.sink.send(LuminaFrame::Welcome(welcome)).await.unwrap();
        let (tx, list_changed) = mpsc::unbounded_channel();
        let transport = (
            self.sink.retype::<rmcp::model::ClientJsonRpcMessage>(),
            self.stream.retype::<rmcp::model::ServerJsonRpcMessage>(),
        );
        let client = Watcher(tx).serve(transport).await.expect("the device serves MCP");
        Mcp { client, list_changed }
    }
}

impl Mcp {
    pub async fn tool_names(&self) -> Vec<String> {
        self.client
            .list_all_tools()
            .await
            .unwrap()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    /// Call `tool` with `purpose` in `_meta`.
    pub async fn call(
        &self,
        tool: &str,
        args: Value,
        purpose: &str,
    ) -> Result<CallToolResult, ErrorData> {
        let mut params = CallToolRequestParams::new(tool.to_owned());
        if let Value::Object(args) = args {
            params.arguments = Some(args);
        }
        let mut meta = rmcp::model::RequestMetaObject::default();
        meta.0 .0.insert(simply_relay::wire::PURPOSE_META_KEY.into(), purpose.into());
        params.meta = Some(meta);
        match self.client.call_tool(params).await {
            Ok(result) => Ok(result),
            Err(rmcp::ServiceError::McpError(e)) => Err(e),
            Err(e) => panic!("transport error: {e}"),
        }
    }
}

/// Wait until the relay's status matches.
pub async fn until(
    relay: &Relay,
    what: impl Fn(&simply_relay::Status) -> bool,
) -> simply_relay::Status {
    let mut watch = relay.watch_status();
    let status = tokio::time::timeout(WAIT, watch.wait_for(|s| what(s)))
        .await
        .unwrap_or_else(|_| panic!("status never matched; last: {:?}", relay.status()))
        .unwrap()
        .clone();
    status
}
