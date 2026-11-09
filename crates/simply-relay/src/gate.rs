//! The app-side gate: what Lumina may see and call, whatever it asks.
//!
//! [`Gated`] wraps the app's own `ServerHandler`. `tools/list` shows only
//! tools the app declared whose class is switched on, plus the standard
//! diagnostics while diagnostics are on. `tools/call` refuses everything else
//! **before** the app's handler sees it, and writes every call, allowed or
//! refused, to the member's call log. Only tools are exposed: prompts,
//! resources and the rest answer "method not found", whatever the app's
//! handler offers on its own transports.
//!
//! A refusal is a JSON-RPC error with code `-32602` (what MCP answers for a
//! tool it does not have) and `data` `{"refused": "switched_off" |
//! "not_exposed", "class": …}`, so Lumina can audit it as refused rather than
//! failed.

use std::sync::Arc;
use std::time::Duration;

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CallToolResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ServerCapabilities, ServerInfo, Tool,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler};
use serde_json::{json, Value};

use crate::calls::{now_rfc3339, CallRecord, Verdict};
use crate::diag;
use crate::store::Class;
use crate::wire::PURPOSE_META_KEY;
use crate::Shared;

/// How long `lumina_report` gives each of the app's own checks.
pub const REPORT_TOOL_TIMEOUT: Duration = Duration::from_secs(10);

/// The app's handler behind the member's switches.
pub struct Gated<S> {
    inner: S,
    shared: Arc<Shared>,
}

impl<S: ServerHandler> Gated<S> {
    pub(crate) fn new(inner: S, shared: Arc<Shared>) -> Self {
        Self { inner, shared }
    }

    /// The tool's class, if it is exposed at all.
    fn class_of(&self, tool: &str) -> Option<Class> {
        if diag::TOOLS.contains(&tool) {
            return Some(Class::ReadOnly);
        }
        self.shared.classes.get(tool).cloned()
    }

    fn allowed(&self, tool: &str) -> bool {
        let consent = self.shared.consent();
        self.class_of(tool).is_some_and(|c| consent.allows(&c))
    }

    fn log(&self, tool: &str, class: Option<&Class>, verdict: Verdict, purpose: Option<&str>) {
        self.shared.calls.append(&CallRecord {
            at: now_rfc3339(),
            tool: tool.to_owned(),
            class: class.map_or("none", Class::name).to_owned(),
            verdict,
            purpose: purpose.map(str::to_owned),
        });
    }

    async fn diagnostic(
        &self,
        tool: &str,
        request: &CallToolRequestParams,
        context: &RequestContext<RoleServer>,
        purpose: Option<&str>,
    ) -> Value {
        match tool {
            diag::ABOUT => self.shared.about.about(),
            diag::CONFIG => json!({ "config": self.shared.config() }),
            diag::ERRORS => json!({ "errors": diag::recent_errors() }),
            diag::LOG => {
                let lines = request
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("lines"))
                    .and_then(Value::as_u64)
                    .map_or(diag::REPORT_LOG_LINES, |n| {
                        n.clamp(1, diag::LOG_LINES as u64) as usize
                    });
                json!({ "lines": diag::recent_log(lines) })
            }
            _ => json!({
                "about": self.shared.about.about(),
                "config": self.shared.config(),
                "errors": diag::recent_errors(),
                "log": diag::recent_log(diag::REPORT_LOG_LINES),
                "checks": self.run_checks(context, purpose).await,
            }),
        }
    }

    /// Each declared read-only app tool that needs no argument, run once.
    async fn run_checks(
        &self,
        context: &RequestContext<RoleServer>,
        purpose: Option<&str>,
    ) -> Value {
        let listed = match self.inner.list_tools(None, context.clone()).await {
            Ok(listed) => listed.tools,
            Err(e) => return json!({ "error": e.message }),
        };
        let mut checks = serde_json::Map::new();
        for tool in listed {
            let name = tool.name.to_string();
            if self.shared.classes.get(&name) != Some(&Class::ReadOnly) || !takes_no_argument(&tool)
            {
                continue;
            }
            self.log(&name, Some(&Class::ReadOnly), Verdict::Allowed, purpose);
            let call =
                self.inner.call_tool(CallToolRequestParams::new(name.clone()), context.clone());
            let value = match tokio::time::timeout(REPORT_TOOL_TIMEOUT, call).await {
                Ok(Ok(response)) => response_value(response),
                Ok(Err(e)) => json!({ "error": e.message }),
                Err(_) => json!({ "error": "timed out" }),
            };
            checks.insert(name, value);
        }
        Value::Object(checks)
    }
}

/// Whether a tool can be called with no arguments.
fn takes_no_argument(tool: &Tool) -> bool {
    tool.input_schema
        .get("required")
        .and_then(Value::as_array)
        .is_none_or(|required| required.is_empty())
}

/// A tool result as one JSON value for the report.
fn response_value(response: CallToolResponse) -> Value {
    let CallToolResponse::Complete(result) = response else {
        return json!({ "error": "the tool did not finish in one call" });
    };
    let text = || {
        result
            .content
            .iter()
            .filter_map(|c| c.as_text().map(|t| t.text.as_str()))
            .collect::<Vec<_>>()
            .join("\n")
    };
    if result.is_error == Some(true) {
        return json!({ "error": text() });
    }
    if let Some(structured) = &result.structured_content {
        return structured.clone();
    }
    let text = text();
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

fn purpose_of(
    request: &CallToolRequestParams,
    context: &RequestContext<RoleServer>,
) -> Option<String> {
    let from = |meta: &rmcp::model::RequestMetaObject| {
        meta.0 .0.get(PURPOSE_META_KEY).and_then(Value::as_str).map(str::to_owned)
    };
    request.meta.as_ref().and_then(from).or_else(|| from(&context.meta))
}

impl<S: ServerHandler> ServerHandler for Gated<S> {
    fn get_info(&self) -> ServerInfo {
        let mut info = self.inner.get_info();
        info.capabilities =
            ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build();
        info
    }

    fn supported_protocol_versions(&self) -> std::borrow::Cow<'static, [ProtocolVersion]> {
        self.inner.supported_protocol_versions()
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let first_page = request.as_ref().and_then(|r| r.cursor.as_ref()).is_none();
        let mut listed = self.inner.list_tools(request, context).await?;
        listed.tools.retain(|t| self.allowed(&t.name));
        if first_page && self.shared.consent().read_only {
            let mut tools = diag::tools();
            tools.append(&mut listed.tools);
            listed.tools = tools;
        }
        Ok(listed)
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        if !self.allowed(name) {
            return None;
        }
        if diag::TOOLS.contains(&name) {
            return diag::tools().into_iter().find(|t| t.name == name);
        }
        self.inner.get_tool(name)
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let tool = request.name.to_string();
        let purpose = purpose_of(&request, &context);
        let class = self.class_of(&tool);
        let consent = self.shared.consent();
        let Some(class) = class.clone().filter(|c| consent.allows(c)) else {
            self.log(&tool, class.as_ref(), Verdict::Refused, purpose.as_deref());
            let app = &self.shared.app;
            let (reason, message) = match &class {
                None => ("not_exposed", format!("refused: {app} does not offer {tool} to Lumina")),
                Some(Class::ReadOnly) => {
                    ("switched_off", format!("refused: diagnostics are switched off in {app}"))
                }
                Some(Class::Action(name)) => {
                    ("switched_off", format!("refused: {name} actions are switched off in {app}"))
                }
            };
            return Err(ErrorData::invalid_params(
                message,
                Some(json!({ "refused": reason, "class": class.as_ref().map(Class::name) })),
            ));
        };
        self.log(&tool, Some(&class), Verdict::Allowed, purpose.as_deref());
        if diag::TOOLS.contains(&tool.as_str()) {
            let value = self.diagnostic(&tool, &request, &context, purpose.as_deref()).await;
            return Ok(CallToolResult::structured(value).into());
        }
        self.inner.call_tool(request, context).await
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        self.shared.set_peer(Some(context.peer.clone()));
        self.inner.on_initialized(context).await;
    }

    async fn on_cancelled(
        &self,
        notification: rmcp::model::CancelledNotificationParam,
        context: NotificationContext<RoleServer>,
    ) {
        self.inner.on_cancelled(notification, context).await;
    }
}
