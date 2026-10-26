//! Exercise the real OpenAI SSE transport, engine, built-in Write tool, and
//! approval protocol against a synthetic localhost provider. No API credential
//! is read from the environment and every tool destination is a temporary file.

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use aion_agent::bootstrap::AgentBootstrap;
use aion_agent::output::null_sink::NullSink;
use aion_agent::tool_policy::ToolPolicy;
use aion_config::compat::ProviderCompat;
use aion_config::config::{Config, ProviderType, SessionConfig, ToolsConfig};
use aion_protocol::events::{ProtocolEvent, ToolCategory};
use aion_protocol::writer::ProtocolEmitter;
use aion_protocol::{ToolApprovalManager, ToolApprovalResult};
use axum::Json;
use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::provider_for;

const WRITTEN_CONTENT: &str = "Synthetic DSML integration test.\n第二行\n";
const DENIAL_REASON: &str = "Synthetic user rejected this write";

struct SyntheticProvider {
    responses: Vec<String>,
    requests: Mutex<Vec<Value>>,
}

async fn respond(State(state): State<Arc<SyntheticProvider>>, Json(request): Json<Value>) -> Response {
    let index = {
        let mut requests = state.requests.lock().unwrap();
        let index = requests.len();
        // Never retain a system prompt discovered by SDK bootstrap. Assertions
        // only need the advertised tools and synthetic conversation messages.
        let mut captured = request;
        if let Some(messages) = captured.get_mut("messages").and_then(Value::as_array_mut) {
            messages.retain(|message| message["role"] != "system");
        }
        requests.push(captured);
        index
    };
    let Some(body) = state.responses.get(index) else {
        return (StatusCode::BAD_REQUEST, "Unexpected extra model request").into_response();
    };
    ([(header::CONTENT_TYPE, "text/event-stream")], body.clone()).into_response()
}

/// Dropping this on assertion failure or timeout also closes the listener.
struct ServerTask(JoinHandle<()>);

impl Drop for ServerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn sse_response(text: &str) -> String {
    let mut body = String::new();
    // Split inside DSML tags and parameter names, including Unicode delimiters.
    // Each fragment travels through the SDK's actual SSE parser.
    for fragment in text.chars().collect::<Vec<_>>().chunks(7) {
        let fragment: String = fragment.iter().collect();
        body.push_str(&format!(
            "data: {}\n\n",
            json!({
                "id": "synthetic-dsml-response",
                "object": "chat.completion.chunk",
                "model": "deepseek-v4.1-flash",
                "choices": [{"index": 0, "delta": {"content": fragment}, "finish_reason": null}]
            })
        ));
    }
    body.push_str(&format!(
        "data: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}),
        json!({"choices": [], "usage": {"prompt_tokens": 25, "completion_tokens": 10}})
    ));
    body
}

fn config(base_url: &str, workspace: &Path) -> Config {
    Config {
        provider_label: "openai".into(),
        provider: ProviderType::OpenAI,
        api_key: "synthetic-localhost-key".into(),
        base_url: base_url.into(),
        model: "deepseek-v4.1-flash".into(),
        max_tokens: Some(1024),
        max_turns: Some(3),
        max_tool_call_malformed_turns: Some(1),
        max_tool_call_failure_turns: Some(2),
        system_prompt: Some("Synthetic local compatibility test.".into()),
        thinking: None,
        prompt_caching: false,
        compat: ProviderCompat::openai_defaults(),
        tools: ToolsConfig {
            auto_approve: false,
            allow_list: Vec::new(),
            skills: Default::default(),
        },
        session: SessionConfig {
            enabled: false,
            directory: workspace.join("sessions").to_string_lossy().into_owned(),
            max_sessions: 1,
        },
        compact: aion_config::compact::CompactConfig {
            enabled: false,
            ..Default::default()
        },
        plan: aion_config::plan::PlanConfig {
            enabled: false,
            plan_directory: workspace.join("plans").to_string_lossy().into_owned(),
        },
        shell: Default::default(),
        file_cache: Default::default(),
        hooks: Default::default(),
        bedrock: None,
        vertex: None,
        mcp: Default::default(),
        logging: Default::default(),
    }
}

struct ApprovalRequest {
    call_id: String,
    name: String,
    category: ToolCategory,
    args: Value,
}

struct CaptureProtocol {
    approvals: mpsc::UnboundedSender<ApprovalRequest>,
    events: Mutex<Vec<Value>>,
}

impl ProtocolEmitter for CaptureProtocol {
    fn emit(&self, event: &ProtocolEvent) -> io::Result<()> {
        self.events.lock().unwrap().push(serde_json::to_value(event)?);
        if let ProtocolEvent::ToolRequest { call_id, tool, .. } = event {
            self.approvals
                .send(ApprovalRequest {
                    call_id: call_id.clone(),
                    name: tool.name.clone(),
                    category: tool.category,
                    args: tool.args.clone(),
                })
                .map_err(|_| io::Error::other("Synthetic approval receiver closed"))?;
        }
        Ok(())
    }
}

async fn run_write_round_trip(approved: bool) {
    let workspace = tempfile::tempdir().unwrap();
    // Stop project-context discovery at this isolated workspace.
    std::fs::create_dir(workspace.path().join(".git")).unwrap();
    let destination = workspace.path().join("dsml-output.txt");
    let destination_text = destination.to_string_lossy().into_owned();
    let dsml = format!(
        "<｜DSML｜ calls><｜DSML｜ invoke name=\"Write\">\n\
         <｜DSML｜ parameter name=\"file_path\" string=\"true\">{destination_text}</｜DSML｜ parameter>\n\
         <｜DSML｜ parameter name=\"content\" string=\"true\">{WRITTEN_CONTENT}</｜DSML｜ parameter>\n\
         </｜DSML｜ invoke></｜DSML｜ calls>"
    );
    let state = Arc::new(SyntheticProvider {
        responses: vec![sse_response(&dsml), sse_response("Synthetic turn finished.")],
        requests: Mutex::new(Vec::new()),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1", listener.local_addr().unwrap());
    let app = axum::Router::new()
        .route("/v1/chat/completions", post(respond))
        .with_state(state.clone());
    let _server = ServerTask(tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    }));

    let config = config(&base_url, workspace.path());
    let provider = provider_for(&config);
    let mut engine = AgentBootstrap::new(
        config,
        workspace.path().to_string_lossy().into_owned(),
        Arc::new(NullSink),
    )
    .provider(provider)
    // This also excludes ExecCommand, Spawn, Skill, and every MCP tool.
    .tool_policy(ToolPolicy::allow_only(["Write"]))
    .build()
    .await
    .expect("Synthetic engine should bootstrap");
    let approval_manager = Arc::new(ToolApprovalManager::new());
    let (approval_tx, mut approval_rx) = mpsc::unbounded_channel();
    let protocol = Arc::new(CaptureProtocol {
        approvals: approval_tx,
        events: Mutex::new(Vec::new()),
    });
    engine.engine.set_approval_manager(approval_manager.clone());
    engine.engine.set_protocol_writer(protocol.clone());

    let approve = async {
        let request = approval_rx.recv().await.expect("Write must request ordinary approval");
        assert_eq!(request.name, "Write");
        assert_eq!(request.category, ToolCategory::Edit);
        assert_eq!(
            request.args,
            json!({"file_path": destination_text, "content": WRITTEN_CONTENT})
        );
        assert!(!destination.exists(), "Write must not run before approval");
        // join! polls the engine until it awaits its pending approval, so this
        // resolution cannot race registration of the SDK's approval channel.
        approval_manager.resolve(
            &request.call_id,
            if approved {
                ToolApprovalResult::Approved
            } else {
                ToolApprovalResult::Denied {
                    reason: DENIAL_REASON.into(),
                }
            },
        );
        request.call_id
    };
    let (result, call_id) = tokio::join!(
        engine.engine.run("Run the synthetic Write fixture.", "synthetic-msg"),
        approve
    );
    let result = result.expect("The engine should consume the tool result and complete its next turn");
    assert_eq!(result.text, "Synthetic turn finished.");
    assert_eq!(result.turns, 2);
    assert!(approval_rx.try_recv().is_err(), "A DSML call must not execute twice");

    let requests = state.requests.lock().unwrap();
    assert_eq!(
        requests.len(),
        2,
        "A real tool result must drive the second model request"
    );
    let advertised = requests[0]["tools"].as_array().unwrap();
    assert_eq!(advertised.len(), 1);
    assert_eq!(advertised[0]["function"]["name"], "Write");
    let messages = requests[1]["messages"].as_array().unwrap();
    let calls = messages
        .iter()
        .filter_map(|message| message.get("tool_calls").and_then(Value::as_array))
        .flatten()
        .collect::<Vec<_>>();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0]["id"], call_id);
    assert_eq!(calls[0]["function"]["name"], "Write");
    let returned = messages
        .iter()
        .filter(|message| message["role"] == "tool")
        .collect::<Vec<_>>();
    assert_eq!(returned.len(), 1);
    assert_eq!(returned[0]["tool_call_id"], call_id);
    let returned_content = returned[0]["content"].as_str().unwrap();
    let events = protocol.events.lock().unwrap();
    if approved {
        assert_eq!(std::fs::read_to_string(&destination).unwrap(), WRITTEN_CONTENT);
        assert!(returned_content.contains("Created"));
        assert!(
            events
                .iter()
                .any(|event| event["type"] == "tool_result" && event["status"] == "success")
        );
    } else {
        assert!(!destination.exists(), "Rejected DSML must never write a file");
        assert!(returned_content.contains(DENIAL_REASON));
        assert!(events.iter().any(|event| event["type"] == "tool_cancelled"));
        assert!(!events.iter().any(|event| event["type"] == "tool_running"));
    }
}

#[tokio::test]
async fn dsml_openai_sse_approved_write_reaches_engine_and_returns_real_tool_result() {
    tokio::time::timeout(Duration::from_secs(20), run_write_round_trip(true))
        .await
        .expect("Synthetic DSML round trip timed out");
}

#[tokio::test]
async fn dsml_openai_sse_rejected_write_preserves_approval_and_returns_denial() {
    tokio::time::timeout(Duration::from_secs(20), run_write_round_trip(false))
        .await
        .expect("Synthetic DSML rejection round trip timed out");
}
