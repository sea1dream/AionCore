use super::*;
use aion_types::message::TokenUsage;
use aion_types::tool::ToolDef;
use serde_json::json;

const BLOCK: &str = "<｜DSML｜ calls>\n<｜DSML｜ invoke name=\"Write\">\n<｜DSML｜ parameter name=\"content\" string=\"true\">你好 &lt;\n code</｜DSML｜ parameter>\n</｜DSML｜ invoke>\n</｜DSML｜ calls>";

struct Fake(Vec<LlmEvent>);

#[async_trait]
impl LlmProvider for Fake {
    async fn stream(&self, _: &LlmRequest) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        let (tx, rx) = mpsc::channel(1024);
        for event in &self.0 {
            tx.try_send(event.clone()).unwrap();
        }
        Ok(rx)
    }
}

fn request() -> LlmRequest {
    LlmRequest {
        model: "deepseek-v4.1-flash".into(),
        system: String::new(),
        messages: vec![],
        tools: vec![ToolDef {
            name: "Write".into(),
            description: String::new(),
            deferred: false,
            input_schema: json!({"type":"object","properties":{"content":{"type":"string"}},"required":["content"],"additionalProperties":false}),
        }],
        max_tokens: None,
        thinking: None,
        reasoning_effort: None,
    }
}

fn done(reason: StopReason) -> LlmEvent {
    LlmEvent::Done {
        stop_reason: reason,
        usage: TokenUsage {
            input_tokens: 21,
            output_tokens: 34,
            ..Default::default()
        },
    }
}

async fn collect(events: Vec<LlmEvent>, request: &LlmRequest) -> Vec<LlmEvent> {
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let mut rx = DsmlCompatProvider::new(Arc::new(Fake(events)))
            .stream(request)
            .await
            .unwrap();
        let mut result = vec![];
        while let Some(event) = rx.recv().await {
            result.push(event);
        }
        result
    })
    .await
    .expect("bounded compatibility stream")
}

fn text(events: &[LlmEvent]) -> String {
    events
        .iter()
        .filter_map(|e| {
            if let LlmEvent::TextDelta(s) = e {
                Some(s.as_str())
            } else {
                None
            }
        })
        .collect()
}

fn calls(events: &[LlmEvent]) -> usize {
    events.iter().filter(|e| matches!(e, LlmEvent::ToolUse { .. })).count()
}

#[tokio::test]
async fn every_unicode_split_recovers_once_and_preserves_usage() {
    let source = format!("正在准备。\n{BLOCK}");
    for split in source.char_indices().map(|(i, _)| i).chain([source.len()]) {
        let events = collect(
            vec![
                LlmEvent::TextDelta(source[..split].into()),
                LlmEvent::TextDelta(source[split..].into()),
                done(StopReason::EndTurn),
            ],
            &request(),
        )
        .await;
        assert_eq!(calls(&events), 1, "split {split}");
        assert_eq!(text(&events), "正在准备。\n");
        assert!(events.iter().any(|e| matches!(e, LlmEvent::ToolUse { id, name, input, extra: None } if !id.is_empty() && name == "Write" && input["content"] == "你好 &lt;\n code")));
        assert!(
            matches!(events.last(), Some(LlmEvent::Done { stop_reason: StopReason::ToolUse, usage }) if usage.input_tokens == 21 && usage.output_tokens == 34)
        );
    }
}

#[tokio::test]
async fn fences_inline_quotes_indentation_and_escaped_examples_stay_text() {
    for source in [
        format!("```xml\n{BLOCK}\n```"),
        format!("~~~\n{BLOCK}\n~~~"),
        format!("`{BLOCK}`"),
        format!("> {BLOCK}"),
        format!("    {BLOCK}"),
        format!("\t{BLOCK}"),
        format!("\\{BLOCK}"),
        format!("Example: {BLOCK}"),
    ] {
        let events = collect(
            source
                .chars()
                .map(|c| LlmEvent::TextDelta(c.to_string()))
                .chain([done(StopReason::EndTurn)])
                .collect(),
            &request(),
        )
        .await;
        assert_eq!(calls(&events), 0);
        assert_eq!(text(&events), source);
    }
}

#[tokio::test]
async fn closed_fence_allows_subsequent_standalone_call() {
    for fence in ["```", "~~~~", "  ````"] {
        let prefix = format!("{fence}\nexample\n{fence}\n");
        let source = format!("{prefix}{BLOCK}");
        let events = collect(
            source
                .chars()
                .map(|c| LlmEvent::TextDelta(c.to_string()))
                .chain([done(StopReason::EndTurn)])
                .collect(),
            &request(),
        )
        .await;
        assert_eq!(calls(&events), 1);
        assert_eq!(text(&events), prefix);
    }
}

#[tokio::test]
async fn incomplete_error_truncated_and_late_content_never_execute() {
    for suffix in [
        vec![],
        vec![done(StopReason::MaxTokens)],
        vec![done(StopReason::MaxTurns)],
        vec![LlmEvent::Error("synthetic failure".into())],
        vec![done(StopReason::EndTurn), LlmEvent::Error("late failure".into())],
        vec![done(StopReason::EndTurn), LlmEvent::TextDelta("late".into())],
        vec![done(StopReason::EndTurn), LlmEvent::ThinkingDelta("late".into())],
        vec![
            done(StopReason::EndTurn),
            LlmEvent::ProviderItem {
                provider: "test".into(),
                item: json!({}),
            },
        ],
        vec![done(StopReason::MaxTokens), done(StopReason::EndTurn)],
    ] {
        let mut events = vec![LlmEvent::TextDelta(BLOCK.into())];
        events.extend(suffix);
        let output = collect(events, &request()).await;
        assert_eq!(calls(&output), 0);
        assert!(text(&output).starts_with(BLOCK));
    }
    let source = &BLOCK[..BLOCK.rfind("</").unwrap()];
    let events = collect(
        vec![LlmEvent::TextDelta(source.into()), done(StopReason::EndTurn)],
        &request(),
    )
    .await;
    assert_eq!(calls(&events), 0);
    assert_eq!(text(&events), source);
}

#[tokio::test]
async fn native_calls_always_win_including_after_done() {
    let native = LlmEvent::ToolUse {
        id: "native".into(),
        name: "Write".into(),
        input: json!({"content":"native"}),
        extra: Some(json!({"opaque":1})),
    };
    for events in [
        vec![
            native.clone(),
            LlmEvent::TextDelta(BLOCK.into()),
            done(StopReason::EndTurn),
        ],
        vec![
            LlmEvent::TextDelta(BLOCK.into()),
            native.clone(),
            done(StopReason::ToolUse),
        ],
        vec![
            LlmEvent::TextDelta(BLOCK.into()),
            done(StopReason::EndTurn),
            native.clone(),
        ],
    ] {
        let output = collect(events, &request()).await;
        assert_eq!(calls(&output), 1);
        assert_eq!(text(&output), BLOCK);
        assert!(
            output
                .iter()
                .any(|e| matches!(e, LlmEvent::ToolUse { id, extra: Some(_), .. } if id == "native"))
        );
    }
}

#[tokio::test]
async fn gating_unknown_tools_and_oversized_blocks_are_non_executable() {
    for model in [
        "gpt-test",
        "deepseek-v4-flash",
        "deepseek-v4.10",
        "not-deepseek-v4.1-flash",
    ] {
        let mut req = request();
        req.model = model.into();
        let output = collect(vec![LlmEvent::TextDelta(BLOCK.into()), done(StopReason::EndTurn)], &req).await;
        assert_eq!(calls(&output), 0);
        assert_eq!(text(&output), BLOCK);
    }
    let mut req = request();
    req.tools.clear();
    assert_eq!(
        calls(&collect(vec![LlmEvent::TextDelta(BLOCK.into()), done(StopReason::EndTurn)], &req).await),
        0
    );
    for source in [
        BLOCK.replace("Write", "Forbidden"),
        format!("{BLOCK}{}", " ".repeat(parser::MAX_BYTES)),
        format!("{BLOCK}\nExplanation"),
    ] {
        let output = collect(
            vec![LlmEvent::TextDelta(source.clone()), done(StopReason::EndTurn)],
            &request(),
        )
        .await;
        assert_eq!(calls(&output), 0);
        assert_eq!(text(&output), source);
    }
}

#[tokio::test]
async fn thinking_is_not_interpreted() {
    let output = collect(
        vec![
            LlmEvent::ThinkingDelta(BLOCK.into()),
            LlmEvent::TextDelta("answer".into()),
            done(StopReason::EndTurn),
        ],
        &request(),
    )
    .await;
    assert_eq!(calls(&output), 0);
    assert!(matches!(output.first(), Some(LlmEvent::ThinkingDelta(s)) if s == BLOCK));
    assert_eq!(text(&output), "answer");
}

struct Waiting(mpsc::Sender<mpsc::Sender<LlmEvent>>);
#[async_trait]
impl LlmProvider for Waiting {
    async fn stream(&self, _: &LlmRequest) -> Result<mpsc::Receiver<LlmEvent>, ProviderError> {
        let (tx, rx) = mpsc::channel(2);
        self.0.send(tx).await.unwrap();
        Ok(rx)
    }
}

#[tokio::test]
async fn ordinary_text_streams_before_done_and_cancellation_drops_upstream() {
    let (tx, mut observed) = mpsc::channel(1);
    let mut output = DsmlCompatProvider::new(Arc::new(Waiting(tx)))
        .stream(&request())
        .await
        .unwrap();
    let upstream = observed.recv().await.unwrap();
    upstream.send(LlmEvent::TextDelta("hello".into())).await.unwrap();
    assert!(
        matches!(tokio::time::timeout(std::time::Duration::from_secs(2), output.recv()).await.unwrap(), Some(LlmEvent::TextDelta(s)) if s == "hello")
    );
    drop(output);
    tokio::time::timeout(std::time::Duration::from_secs(2), upstream.closed())
        .await
        .unwrap();
}
