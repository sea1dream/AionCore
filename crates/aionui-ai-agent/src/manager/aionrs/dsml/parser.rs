//! Conservative parser for the documented DeepSeek V4.1 DSML tool-call grammar.
//!
//! Reference: https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash/blob/main/encoding/README.md#tool-calling-dsml-format
//! This is not XML: string parameters are raw text, not entity-decoded values.

use aion_types::tool::ToolDef;
use serde_json::{Map, Value};

pub(super) const OPEN: &str = "<｜DSML｜ calls>";
pub(super) const MAX_BYTES: usize = 512 * 1024;

const CLOSE: &str = "</｜DSML｜ calls>";
const INVOKE_OPEN: &str = "<｜DSML｜ invoke name=\"";
const INVOKE_CLOSE: &str = "</｜DSML｜ invoke>";
const PARAMETER_OPEN: &str = "<｜DSML｜ parameter name=\"";
const PARAMETER_CLOSE: &str = "</｜DSML｜ parameter>";
const EOS: &str = "<｜end▁of▁sentence｜>";
const MAX_CALLS: usize = 32;
const MAX_PARAMETERS: usize = 64;
const MAX_NAME_BYTES: usize = 128;

#[derive(Debug, PartialEq)]
pub(super) struct ParsedCall {
    pub name: String,
    pub input: Value,
}

/// Parse the entire candidate atomically; callers must never execute a partial result.
///
/// Markdown context and successful stream termination are checked by the wrapper.
/// All errors are static diagnostic codes, never model output or tool arguments.
pub(super) fn parse(block: &str, tools: &[ToolDef]) -> Result<Vec<ParsedCall>, &'static str> {
    if block.len() > MAX_BYTES {
        return Err("block_too_large");
    }
    let mut remaining = block.strip_prefix(OPEN).ok_or("invalid_calls_open")?;
    let mut calls = Vec::new();

    loop {
        remaining = trim_separator(remaining);
        if let Some(tail) = remaining.strip_prefix(CLOSE) {
            if calls.is_empty() {
                return Err("empty_calls");
            }
            let tail = trim_separator(tail);
            let tail = tail.strip_prefix(EOS).map_or(tail, trim_separator);
            if !tail.is_empty() {
                return Err("trailing_content");
            }
            return Ok(calls);
        }
        if calls.len() >= MAX_CALLS {
            return Err("too_many_calls");
        }
        let (name, tail) = read_name(remaining, INVOKE_OPEN, "invalid_invoke", "invalid_tool_name")?;
        // read_name consumes only the quote. The closing bracket must be exact.
        remaining = tail.strip_prefix('>').ok_or("invalid_invoke")?;

        let mut matching_tools = tools.iter().filter(|tool| tool.name == name);
        let tool = matching_tools.next().ok_or("unknown_tool")?;
        if matching_tools.next().is_some() {
            return Err("duplicate_tool_definition");
        }

        let (input, tail) = parse_parameters(remaining)?;
        if !super::schema::validate(&tool.input_schema, &input) {
            return Err("schema_mismatch");
        }
        calls.push(ParsedCall {
            name: name.to_owned(),
            input,
        });
        remaining = tail;
    }
}

fn parse_parameters(mut remaining: &str) -> Result<(Value, &str), &'static str> {
    let mut parameters = Map::new();
    loop {
        remaining = trim_separator(remaining);
        if let Some(tail) = remaining.strip_prefix(INVOKE_CLOSE) {
            return Ok((Value::Object(parameters), tail));
        }
        if parameters.len() >= MAX_PARAMETERS {
            return Err("too_many_parameters");
        }
        let (name, tail) = read_name(remaining, PARAMETER_OPEN, "invalid_parameter", "invalid_parameter_name")?;
        if parameters.contains_key(name) {
            return Err("duplicate_parameter");
        }
        let (is_string, value_and_tail) = if let Some(tail) = tail.strip_prefix(" string=\"true\">") {
            (true, tail)
        } else if let Some(tail) = tail.strip_prefix(" string=\"false\">") {
            (false, tail)
        } else {
            return Err("invalid_parameter_type");
        };
        let value_end = value_and_tail.find(PARAMETER_CLOSE).ok_or("unclosed_parameter")?;
        let raw_value = &value_and_tail[..value_end];
        // DSML has no escape convention for its own delimiters. Ambiguous nested
        // markup stays visible as text instead of becoming executable tool input.
        if raw_value.contains("<｜DSML｜") || raw_value.contains("</｜DSML｜") {
            return Err("nested_dsml");
        }
        let value = if is_string {
            Value::String(raw_value.to_owned())
        } else {
            let value: Value = serde_json::from_str(raw_value).map_err(|_| "invalid_json_parameter")?;
            if value.is_string() {
                return Err("json_string_parameter");
            }
            value
        };
        parameters.insert(name.to_owned(), value);
        remaining = &value_and_tail[value_end + PARAMETER_CLOSE.len()..];
    }
}

fn read_name<'a>(
    input: &'a str,
    prefix: &str,
    tag_error: &'static str,
    name_error: &'static str,
) -> Result<(&'a str, &'a str), &'static str> {
    let rest = input.strip_prefix(prefix).ok_or(tag_error)?;
    let name_end = rest.find('"').ok_or(tag_error)?;
    let name = &rest[..name_end];
    if name.is_empty()
        || name.len() > MAX_NAME_BYTES
        || name
            .chars()
            .any(|character| character.is_whitespace() || character.is_control() || matches!(character, '<' | '>'))
    {
        return Err(name_error);
    }
    Ok((name, &rest[name_end + 1..]))
}

fn trim_separator(input: &str) -> &str {
    input.trim_start_matches([' ', '\t', '\r', '\n'])
}

#[cfg(test)]
#[path = "parser_tests.rs"]
mod tests;
