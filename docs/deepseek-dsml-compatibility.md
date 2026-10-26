# DeepSeek V4.1 DSML compatibility

Some OpenAI-compatible gateways return a DeepSeek tool-call block in assistant
`content` instead of the structured `tool_calls` field. The embedded agent now
recognizes the documented V4.1 encoding for `deepseek-v4.1` and
`deepseek-v4.1-*` models (including provider-prefixed model IDs).

Reference grammar: [DeepSeek V4.1 encoding](https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash/blob/main/encoding/README.md#tool-calling-dsml-format).

The provider wrapper converts a validated terminal calls block to ordinary
`LlmEvent::ToolUse` events. The existing engine handles approval, tool policy,
execution, result persistence and the next model request. It does not evaluate
code from message text, scan conversation history, or change permission policy.
Previously saved raw DSML messages are not executed retroactively.

Recovery requires all of the following:

- An OpenAI provider request for a supported model, with declared tools.
- A standalone calls opener at the beginning of a line, optionally preceded by
  up to three spaces, outside Markdown examples. Ordinary prefixes stream normally.
- A complete terminal block, with only whitespace or the documented EOS after it.
- Exact tool names from the current request, unique parameter names and validated
  argument types. String parameters preserve their original text and entities.
- A successful end event and stream closure, without truncation, errors or native
  tool calls. Native tool calls always take precedence to prevent double execution.

Parsing is bounded to 512 KiB, 32 calls and 64 parameters per call. The fallback
schema checker deliberately supports a limited, bounded subset of JSON Schema;
unsupported constraints or references reject recovery, without network/file
resolution. Malformed, ambiguous, unsupported or oversized blocks remain visible
as text. Diagnostics record only static rejection reasons or recovery counts.

Tests include Unicode chunk boundaries, malformed blocks, Markdown examples,
native-call precedence, truncated streams and cancellation. Local HTTP integration
tests pass synthetic SSE through the actual OpenAI provider and agent engine,
then verify that approving Write creates the expected file and rejecting Write
does not execute it; the following request contains the corresponding tool result.
