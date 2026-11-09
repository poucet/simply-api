//! Lumina's MCP client is rmcp 1.3; the apps serve rmcp 3.x. They never share
//! a process, so what has to agree is the wire. This drives the topology the
//! relay builds, end to end over a real WebSocket on loopback:
//!
//! - the **app** side dials out and serves a real `simply_api::McpApiServer`
//!   (rmcp 3.x) over [`simply_relay::frames`], one JSON-RPC message per text
//!   frame;
//! - the **Lumina** side accepts the socket and is the MCP *client*, on
//!   rmcp 1.3 through its own sink/stream transport.
//!
//! It checks `initialize` negotiates `2025-06-18` (rmcp 1.3's newest), that
//! `tools/list` and a structured `tools/call` come through, and that a second
//! connection after the first one drops starts a fresh session.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use simply_api::{api_service, mcp::McpApiServer, ApiResult};
use tokio::net::TcpListener;

use rmcp13::ServiceExt as _;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Levels {
    /// Peak level per channel, dBFS.
    pub peaks: Vec<f32>,
    /// The output device's sample rate.
    pub rate: u32,
}

/// A stand-in for an app's api.
#[api_service]
#[async_trait]
pub trait DemoApi: Send + Sync {
    /// Current levels.
    async fn levels(&self) -> ApiResult<Levels>;

    /// Echo some text back.
    async fn echo(
        &self,
        /// What to echo.
        text: String,
    ) -> ApiResult<String>;
}

struct Demo;

#[async_trait]
impl DemoApi for Demo {
    async fn levels(&self) -> ApiResult<Levels> {
        Ok(Levels { peaks: vec![-6.0, -7.5], rate: 48_000 })
    }
    async fn echo(&self, text: String) -> ApiResult<String> {
        Ok(text)
    }
}

/// The app's half: dial `url` and serve one MCP session until it ends.
async fn app_serves_once(url: String) {
    let (ws, _) = tokio_tungstenite::connect_async(url).await.expect("the app dials Lumina");
    let (sink, stream) = simply_relay::frames::json_frames::<
        _,
        rmcp::model::ServerJsonRpcMessage,
        rmcp::model::ClientJsonRpcMessage,
    >(ws, None);
    let server = McpApiServer::new(Arc::new(Demo), demo_api_tool_router::<Demo>());
    let running = rmcp::serve_server(server, (sink, stream)).await.expect("the app serves");
    let _ = running.waiting().await;
}

/// Lumina's half: accept one socket and connect to it as an rmcp 1.3
/// client.
async fn lumina_accepts(
    listener: &TcpListener,
) -> rmcp13::service::RunningService<rmcp13::RoleClient, ()> {
    let (tcp, _) = listener.accept().await.expect("a device connects");
    let ws = tokio_tungstenite::accept_async(tcp).await.expect("the upgrade succeeds");
    let (sink, stream) = simply_relay::frames::json_frames::<
        _,
        rmcp13::model::ClientJsonRpcMessage,
        rmcp13::model::ServerJsonRpcMessage,
    >(ws, None);
    ().serve((sink, stream)).await.expect("initialize succeeds")
}

#[tokio::test]
async fn an_rmcp_1_3_client_drives_an_rmcp_3_server_over_the_websocket() {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}/devices/v1", listener.local_addr().expect("addr"));

    for session in 0..2 {
        let app = tokio::spawn(app_serves_once(url.clone()));
        let client = lumina_accepts(&listener).await;

        let info = client.peer_info().expect("the server answered initialize");
        assert_eq!(
            info.protocol_version,
            rmcp13::model::ProtocolVersion::V_2025_06_18,
            "session {session}: the 3.x server meets the 1.3 client at its newest revision"
        );
        assert!(info.capabilities.tools.is_some(), "the server offers tools");

        let mut names: Vec<String> = client
            .list_all_tools()
            .await
            .expect("tools/list")
            .into_iter()
            .map(|t| t.name.into())
            .collect();
        names.sort();
        assert_eq!(names, ["echo", "levels"]);

        let levels = client
            .call_tool(rmcp13::model::CallToolRequestParams::new("levels"))
            .await
            .expect("tools/call levels");
        assert_ne!(levels.is_error, Some(true), "{levels:?}");
        assert_eq!(
            levels.structured_content,
            Some(serde_json::json!({ "peaks": [-6.0, -7.5], "rate": 48000 })),
            "the structured result survives the version gap"
        );

        let args = serde_json::json!({ "text": "hello from 1.3" });
        let echo = client
            .call_tool(
                rmcp13::model::CallToolRequestParams::new("echo")
                    .with_arguments(args.as_object().expect("object").clone()),
            )
            .await
            .expect("tools/call echo");
        let text = echo.content.first().and_then(|c| c.as_text()).map(|t| t.text.clone());
        assert_eq!(text.as_deref(), Some("hello from 1.3"));

        // Dropping the client closes Lumina's end; the app's session ends
        // with it, and the next loop is a reconnect: a fresh session.
        client.cancel().await.expect("the client stops");
        tokio::time::timeout(std::time::Duration::from_secs(5), app)
            .await
            .expect("the app notices the socket closed")
            .expect("the app task finishes");
    }
}

/// The whole relay (handshake, rotated token, gate, diagnostics) against
/// Lumina's rmcp 1.3 client: what the listener in phase 2 must do, written
/// out.
#[tokio::test]
async fn the_full_relay_serves_lumina_s_rmcp_1_3_client() {
    use futures::{SinkExt, StreamExt};
    use simply_relay::wire::{AppFrame, Auth, LuminaFrame, Welcome};

    let dir = tempfile::tempdir().expect("a scratch dir");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let url = format!("ws://{}/devices/v1", listener.local_addr().expect("addr"));
    let relay = simply_relay::Relay::builder("demo", "0.1.0", dir.path())
        .endpoint(url)
        .read_only(["levels"])
        .spawn(|| McpApiServer::new(Arc::new(Demo), demo_api_tool_router::<Demo>()))
        .expect("the relay starts");
    relay.set_read_only(true).expect("switch on");
    let pairing = tokio::spawn({
        let relay = relay.clone();
        async move { relay.pair("CODE").await }
    });

    // Lumina: accept, read the hello, answer a welcome, then speak MCP.
    let (tcp, _) = listener.accept().await.expect("a device connects");
    let ws = tokio_tungstenite::accept_async(tcp).await.expect("upgrade");
    let (mut sink, mut stream) =
        simply_relay::frames::json_frames::<_, LuminaFrame, AppFrame>(ws, None);
    let Some(AppFrame::Hello(hello)) = stream.next().await else { panic!("a hello first") };
    assert_eq!(hello.auth, Auth::Pair { pair: "CODE".into() });
    assert!(hello.consent.read_only);
    sink.send(LuminaFrame::Welcome(Welcome {
        device_id: "d-1".into(),
        token: "t-1".into(),
        member: None,
    }))
    .await
    .expect("welcome");
    let client = ()
        .serve((
            sink.retype::<rmcp13::model::ClientJsonRpcMessage>(),
            stream.retype::<rmcp13::model::ServerJsonRpcMessage>(),
        ))
        .await
        .expect("initialize");
    pairing.await.expect("join").expect("paired");

    let mut names: Vec<String> =
        client.list_all_tools().await.expect("list").into_iter().map(|t| t.name.into()).collect();
    names.sort();
    assert_eq!(
        names,
        ["levels", "lumina_about", "lumina_config", "lumina_errors", "lumina_log", "lumina_report"],
        "echo was not declared, so it is not offered"
    );
    let report = client
        .call_tool(rmcp13::model::CallToolRequestParams::new("lumina_report"))
        .await
        .expect("report");
    let report = report.structured_content.expect("structured");
    assert_eq!(report["checks"]["levels"]["rate"], 48000);
    let refused = client
        .call_tool(rmcp13::model::CallToolRequestParams::new("echo"))
        .await
        .expect_err("echo is refused");
    assert!(refused.to_string().contains("does not offer echo"), "{refused}");

    client.cancel().await.expect("stop");
    relay.shutdown().await;
}
