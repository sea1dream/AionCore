use serde_json::json;

use super::*;

fn tool(name: &str, input_schema: Value) -> ToolDef {
    ToolDef {
        name: name.to_owned(),
        description: "DSML parser test tool".to_owned(),
        input_schema,
        deferred: false,
    }
}

fn write_tool() -> ToolDef {
    tool(
        "Write",
        json!({
            "type": "object",
            "properties": {
                "file_path": {"type": "string"},
                "content": {"type": "string"}
            },
            "required": ["file_path", "content"],
            "additionalProperties": false
        }),
    )
}

fn parameter(name: &str, raw_value: &str, is_string: bool) -> String {
    format!("{PARAMETER_OPEN}{name}\" string=\"{is_string}\">{raw_value}{PARAMETER_CLOSE}")
}

fn invoke(name: &str, parameters: &str) -> String {
    format!("{INVOKE_OPEN}{name}\">{parameters}{INVOKE_CLOSE}")
}

fn block(invocations: &str) -> String {
    format!("{OPEN}\n{invocations}\n{CLOSE}")
}

fn write_call(content: &str) -> String {
    invoke(
        "Write",
        &(parameter("file_path", r"D:\safe-test\note.md", true) + &parameter("content", content, true)),
    )
}

#[test]
fn parses_documented_v41_example_with_eos() {
    // Fixture shape follows the first-party V4.1 grammar, not an XML parser:
    // https://huggingface.co/deepseek-ai/DeepSeek-V4.1-Flash/blob/main/encoding/README.md#tool-calling-dsml-format
    let input = concat!(
        "<｜DSML｜ calls>\n",
        "<｜DSML｜ invoke name=\"function_name\">\n",
        "<｜DSML｜ parameter name=\"param\" string=\"true\">string_value</｜DSML｜ parameter>\n",
        "<｜DSML｜ parameter name=\"count\" string=\"false\">5</｜DSML｜ parameter>\n",
        "</｜DSML｜ invoke>\n",
        "</｜DSML｜ calls><｜end▁of▁sentence｜>"
    );
    let tools = [tool(
        "function_name",
        json!({
            "type": "object",
            "properties": {"param": {"type": "string"}, "count": {"type": "integer"}},
            "required": ["param", "count"],
            "additionalProperties": false
        }),
    )];
    assert_eq!(
        parse(input, &tools).unwrap(),
        vec![ParsedCall {
            name: "function_name".to_owned(),
            input: json!({"param": "string_value", "count": 5}),
        }]
    );
}

#[test]
fn raw_strings_preserve_unicode_whitespace_quotes_backslashes_and_xml_entities() {
    let content = "  中文🙂\r\n<article>\"quoted\" &lt; &amp; 'single'</article>\n\\n\\t\\\\  ";
    let parsed = parse(&block(&write_call(content)), &[write_tool()]).unwrap();
    assert_eq!(parsed[0].input["content"], content);
    assert_eq!(parsed[0].input["file_path"], r"D:\safe-test\note.md");
}

#[test]
fn empty_raw_strings_are_valid() {
    let parsed = parse(&block(&write_call("")), &[write_tool()]).unwrap();
    assert_eq!(parsed[0].input["content"], "");
}

#[test]
fn parses_all_non_string_json_types_without_coercion() {
    let parameters = parameter("integer", "-17", false)
        + &parameter("number", "1.25e2", false)
        + &parameter("boolean", "true", false)
        + &parameter("array", " [1, false, null, \"中文\"] ", false)
        + &parameter("object", r#"{"nested":{"key":"value"}}"#, false)
        + &parameter("null", "null", false);
    let calls = parse(
        &block(&invoke("Check", &parameters)),
        &[tool("Check", json!({"type": "object"}))],
    )
    .unwrap();
    assert_eq!(
        calls[0].input,
        json!({
            "integer": -17,
            "number": 125.0,
            "boolean": true,
            "array": [1, false, null, "中文"],
            "object": {"nested": {"key": "value"}},
            "null": null
        })
    );
}

#[test]
fn accepts_multiple_calls_to_the_same_declared_tool_in_order() {
    let calls = parse(&block(&(write_call("first") + &write_call("second"))), &[write_tool()]).unwrap();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].input["content"], "first");
    assert_eq!(calls[1].input["content"], "second");
}

#[test]
fn permits_exact_declared_namespaces_and_empty_parameter_objects() {
    // V4.1 README's "Tool namespaces" section uses namespace::function names.
    let calls = parse(
        &block(&invoke("search::lookup", "")),
        &[tool("search::lookup", json!({"type": "object"}))],
    )
    .unwrap();
    assert_eq!(calls[0].name, "search::lookup");
    assert_eq!(calls[0].input, json!({}));
}

#[test]
fn accepts_ascii_separators_and_optional_single_eos() {
    let call = write_call("content");
    for ending in ["", " \r\n\t", EOS, &format!(" \n{EOS}\r\n\t ")] {
        let input = format!("{OPEN} \r\n\t{call}\t\r\n {CLOSE}{ending}");
        assert_eq!(parse(&input, &[write_tool()]).unwrap().len(), 1);
    }
}

#[test]
fn rejects_unknown_or_ambiguous_tool_definitions() {
    let input = block(&write_call("content"));
    assert_eq!(parse(&input, &[]).unwrap_err(), "unknown_tool");
    assert_eq!(
        parse(&input, &[tool("write", json!({"type": "object"}))]).unwrap_err(),
        "unknown_tool"
    );
    assert_eq!(
        parse(&input, &[write_tool(), write_tool()]).unwrap_err(),
        "duplicate_tool_definition"
    );
}

#[test]
fn validates_required_properties_property_types_and_additional_properties() {
    let missing_required = invoke("Write", &parameter("content", "content", true));
    let wrong_type = invoke(
        "Write",
        &(parameter("file_path", "42", false) + &parameter("content", "content", true)),
    );
    let unknown_property = invoke(
        "Write",
        &(parameter("file_path", "note.md", true)
            + &parameter("content", "content", true)
            + &parameter("unexpected", "true", false)),
    );
    for call in [missing_required, wrong_type, unknown_property] {
        assert_eq!(parse(&block(&call), &[write_tool()]).unwrap_err(), "schema_mismatch");
    }
}

#[test]
fn rejects_duplicate_parameter_names() {
    let call = invoke(
        "Write",
        &(parameter("content", "first", true) + &parameter("content", "second", true)),
    );
    assert_eq!(
        parse(&block(&call), &[write_tool()]).unwrap_err(),
        "duplicate_parameter"
    );
}

#[test]
fn rejects_invalid_json_and_json_strings_with_false_flag() {
    let tools = [tool("Check", json!({"type": "object"}))];
    for raw_value in [
        "",
        "NaN",
        "Infinity",
        "01",
        "[1,]",
        "{\"a\":}",
        "true false",
        "true trailing",
    ] {
        let input = block(&invoke("Check", &parameter("value", raw_value, false)));
        assert_eq!(
            parse(&input, &tools).unwrap_err(),
            "invalid_json_parameter",
            "case: {raw_value}"
        );
    }
    let input = block(&invoke("Check", &parameter("value", "\"text\"", false)));
    assert_eq!(parse(&input, &tools).unwrap_err(), "json_string_parameter");
}

#[test]
fn rejects_json_nesting_beyond_serde_limit() {
    let raw_value = format!("{}null{}", "[".repeat(130), "]".repeat(130));
    let input = block(&invoke("Check", &parameter("value", &raw_value, false)));
    assert_eq!(
        parse(&input, &[tool("Check", json!({"type": "object"}))]).unwrap_err(),
        "invalid_json_parameter"
    );
}

#[test]
fn rejects_nested_reserved_dsml_tags_in_parameter_bodies() {
    for content in [
        "<｜DSML｜ calls>",
        "</｜DSML｜ invoke>",
        "<｜DSML｜ invoke name=\"Write\">",
        "<｜DSML｜ parameter name=\"x\">",
    ] {
        assert_eq!(
            parse(&block(&write_call(content)), &[write_tool()]).unwrap_err(),
            "nested_dsml"
        );
    }
}

#[test]
fn rejects_malformed_tags_without_attempting_repair() {
    let tools = [tool("Check", json!({"type": "object"}))];
    let cases = [
        (
            "<｜DSML｜tool_calls></｜DSML｜tool_calls>".to_owned(),
            "invalid_calls_open",
        ),
        (format!(" {OPEN}{CLOSE}"), "invalid_calls_open"),
        (format!("{OPEN}{CLOSE}"), "empty_calls"),
        (OPEN.to_owned(), "invalid_invoke"),
        (
            block("<｜DSML｜ invoke name='Check'></｜DSML｜ invoke>"),
            "invalid_invoke",
        ),
        (
            block("<｜DSML｜ invoke name=\"Check\" extra=\"x\"></｜DSML｜ invoke>"),
            "invalid_invoke",
        ),
        (block("<｜DSML｜ invoke name=\"Check\"/>"), "invalid_invoke"),
        (block("<｜DSML｜ invoke name=\"Check\">"), "invalid_parameter"),
        (block(&invoke("Check", "non-tag text")), "invalid_parameter"),
        (
            block(&invoke(
                "Check",
                "<｜DSML｜ parameter name=\"x\" string=\"TRUE\">1</｜DSML｜ parameter>",
            )),
            "invalid_parameter_type",
        ),
        (
            block(&invoke("Check", "<｜DSML｜ parameter name=\"x\" string=\"true\">text")),
            "unclosed_parameter",
        ),
        (format!("{OPEN}{}", invoke("Check", "")), "invalid_invoke"),
    ];
    for (input, expected) in cases {
        assert_eq!(parse(&input, &tools).unwrap_err(), expected, "case: {input}");
    }
}

#[test]
fn rejects_suffix_text_repeated_eos_and_second_calls_block() {
    let valid = block(&write_call("content"));
    for suffix in ["answer", "\n```", &format!("{EOS}{EOS}"), &valid] {
        assert_eq!(
            parse(&format!("{valid}{suffix}"), &[write_tool()]).unwrap_err(),
            "trailing_content"
        );
    }
}

#[test]
fn rejects_entire_batch_when_a_later_call_is_invalid() {
    let valid = write_call("first");
    let invalid = invoke("Unknown", "");
    assert_eq!(
        parse(&block(&(valid + &invalid)), &[write_tool()]).unwrap_err(),
        "unknown_tool"
    );
}

#[test]
fn enforces_call_count_limit() {
    let tools = [tool("Check", json!({"type": "object"}))];
    let call = invoke("Check", "");
    assert_eq!(parse(&block(&call.repeat(MAX_CALLS)), &tools).unwrap().len(), MAX_CALLS);
    assert_eq!(
        parse(&block(&call.repeat(MAX_CALLS + 1)), &tools).unwrap_err(),
        "too_many_calls"
    );
}

#[test]
fn enforces_parameter_count_limit() {
    let tools = [tool("Check", json!({"type": "object"}))];
    let parameters: String = (0..MAX_PARAMETERS)
        .map(|index| parameter(&format!("p{index}"), "value", true))
        .collect();
    let input = block(&invoke("Check", &parameters));
    assert_eq!(
        parse(&input, &tools).unwrap()[0].input.as_object().unwrap().len(),
        MAX_PARAMETERS
    );
    let extra = parameters + &parameter("one_more", "value", true);
    assert_eq!(
        parse(&block(&invoke("Check", &extra)), &tools).unwrap_err(),
        "too_many_parameters"
    );
}

#[test]
fn enforces_name_limits_and_rejects_control_or_markup_characters() {
    let longest = "a".repeat(MAX_NAME_BYTES);
    let tools = [tool(&longest, json!({"type": "object"}))];
    let input = block(&invoke(&longest, &parameter(&longest, "value", true)));
    assert_eq!(parse(&input, &tools).unwrap()[0].name, longest);

    let names = [
        String::new(),
        "two words".to_owned(),
        "tab\tname".to_owned(),
        "line\nname".to_owned(),
        "<name>".to_owned(),
        "a\0b".to_owned(),
        "a".repeat(MAX_NAME_BYTES + 1),
    ];
    for name in names {
        assert_eq!(
            parse(&block(&invoke(&name, "")), &tools).unwrap_err(),
            "invalid_tool_name"
        );
        let input = block(&invoke(&longest, &parameter(&name, "value", true)));
        assert_eq!(parse(&input, &tools).unwrap_err(), "invalid_parameter_name");
    }
}

#[test]
fn enforces_byte_limit_before_parsing_or_allocating_argument_values() {
    let overhead = block(&write_call("")).len();
    let content = "a".repeat(MAX_BYTES - overhead);
    let at_limit = block(&write_call(&content));
    assert_eq!(at_limit.len(), MAX_BYTES);
    assert_eq!(
        parse(&at_limit, &[write_tool()]).unwrap()[0].input["content"]
            .as_str()
            .unwrap()
            .len(),
        content.len()
    );
    assert_eq!(
        parse(&(at_limit + " "), &[write_tool()]).unwrap_err(),
        "block_too_large"
    );
}
