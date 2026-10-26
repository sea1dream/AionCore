//! Recover DeepSeek V4.1 DSML leaked into OpenAI content, through normal tool approval.
//!
//! Only a standalone terminal calls block is eligible. Quoted/indented/fenced examples,
//! unsupported schemas and incomplete responses remain text. No historical text is replayed.

mod parser;
mod schema;
mod text;

use std::sync::Arc;

use aion_config::config::{Config, ProviderType};
use aion_providers::{LlmProvider, ProviderError, create_provider};
use aion_types::llm::{LlmEvent, LlmRequest};
use aion_types::message::StopReason;
use async_trait::async_trait;
use tokio::sync::mpsc;

pub(super) fn provider_for(config: &Config) -> Arc<dyn LlmProvider> {
    let inner = create_provider(config);
    if matches!(config.provider, ProviderType::OpenAI) {
        Arc::new(DsmlCompatProvider::new(inner))
    } else {
        inner
    }
}

pub(super) struct DsmlCompatProvider {
    inner: Arc<dyn LlmProvider>,
}

impl DsmlCompatProvider {
    pub(super) fn new(inner: Arc<dyn LlmProvider>) -> Self {
        Self { inner }
    }
}

fn enabled(request: &LlmRequest) -> bool {
    let model = request
        .model
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    !request.tools.is_empty() && (model == "deepseek-v4.1" || model.starts_with("deepseek-v4.1-"))
}

#[async_trait]
impl LlmProvider for DsmlCompatProvider {
    async fn stream(&self, request: &LlmRequest) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        let mut upstream = self.inner.stream(request).await?;
        if !enabled(request) {
            return Ok(upstream);
        }
        let tools = request.tools.clone();
        let (tx, rx) = mpsc::channel(32);
        tokio::spawn(async move {
            let mut text = text::TextBuffer::default();
            let mut done = None;
            let mut eligible = true;
            loop {
                let event = tokio::select! {
                    biased;
                    _ = tx.closed() => return,
                    event = upstream.recv() => event,
                };
                let Some(event) = event else { break };
                match event {
                    LlmEvent::TextDelta(delta) => {
                        if done.is_some() {
                            eligible = false;
                        }
                        let plain = text.push(&delta);
                        if !send_text(&tx, plain).await {
                            return;
                        }
                    }
                    LlmEvent::Done { stop_reason, usage } => {
                        if done.is_some() || !matches!(stop_reason, StopReason::EndTurn | StopReason::ToolUse) {
                            eligible = false;
                        }
                        // The SDK consumes until EOF. Do not release recovered tools on the
                        // first Done: a later native call, error or truncation must win.
                        done = Some((stop_reason, usage));
                    }
                    other => {
                        if done.is_some() {
                            eligible = false;
                        }
                        if matches!(other, LlmEvent::ToolUse { .. } | LlmEvent::Error(_)) {
                            eligible = false;
                            if !send_text(&tx, text.disable()).await {
                                return;
                            }
                        }
                        if tx.send(other).await.is_err() {
                            return;
                        }
                    }
                }
            }
            let candidate = text.finish();
            let recovered = if eligible && done.is_some() && candidate.starts_with(parser::OPEN) {
                match parser::parse(&candidate, &tools) {
                    Ok(calls) => Some(calls),
                    Err(reason) => {
                        tracing::warn!(reason, "DSML compatibility left an unvalidated block as text");
                        None
                    }
                }
            } else {
                None
            };
            if let Some(calls) = recovered {
                tracing::info!(call_count = calls.len(), "Recovered DeepSeek V4.1 DSML tool calls");
                for call in calls {
                    if tx
                        .send(LlmEvent::ToolUse {
                            id: format!("dsml_{}", uuid::Uuid::new_v4().simple()),
                            name: call.name,
                            input: call.input,
                            extra: None,
                        })
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
                if let Some((_, usage)) = done {
                    let _ = tx
                        .send(LlmEvent::Done {
                            stop_reason: StopReason::ToolUse,
                            usage,
                        })
                        .await;
                }
            } else {
                if !send_text(&tx, candidate).await {
                    return;
                }
                if let Some((stop_reason, usage)) = done {
                    let _ = tx.send(LlmEvent::Done { stop_reason, usage }).await;
                }
            }
        });
        Ok(rx)
    }
}

async fn send_text(tx: &mpsc::Sender<LlmEvent>, text: String) -> bool {
    text.is_empty() || tx.send(LlmEvent::TextDelta(text)).await.is_ok()
}

#[cfg(test)]
mod integration_tests;
#[cfg(test)]
mod tests;
