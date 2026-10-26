use super::*;
use serde_json::json;

#[test]
fn accepts_builtin_write_schema_and_checks_required_argument_types() {
    // Matches the actual aion-tools Write definition (write.rs:47–61).
    let schema = json!({
        "type": "object",
        "properties": {
            "file_path": {"type": "string", "description": "Absolute path"},
            "content": {"type": "string", "description": "File contents"}
        },
        "required": ["file_path", "content"]
    });
    assert!(validate(
        &schema,
        &json!({"file_path":"example.txt", "content":"中文\n<code>"})
    ));
    assert!(!validate(&schema, &json!({"file_path":"example.txt"})));
    assert!(!validate(&schema, &json!({"file_path":"example.txt", "content":false})));
}

#[test]
fn additional_properties_are_checked_without_changing_the_default() {
    let open = json!({"type":"object", "properties":{"name":{"type":"string"}}});
    assert!(validate(&open, &json!({"name":"ok", "other":1})));
    let mut closed = open.clone();
    closed["additionalProperties"] = json!(false);
    assert!(!validate(&closed, &json!({"name":"ok", "other":1})));
    closed["additionalProperties"] = json!({"type":"integer"});
    assert!(validate(&closed, &json!({"name":"ok", "other":1})));
    assert!(!validate(&closed, &json!({"name":"ok", "other":"bad"})));
}

#[test]
fn unions_and_json_numeric_equality_follow_schema_semantics() {
    assert!(validate(&json!({"type":["string","null"]}), &Value::Null));
    assert!(!validate(&json!({"type":["string","null"]}), &json!(7)));
    assert!(validate(&json!({"type":"integer"}), &json!(1.0)));
    assert!(!validate(&json!({"type":"integer"}), &json!(1.5)));
    assert!(validate(&json!({"const":{"x":[1]}}), &json!({"x":[1.0]})));
    assert!(validate(&json!({"enum":[1,"one"]}), &json!(1.0)));
    assert!(!validate(&json!({"enum":[1,1.0]}), &json!(1)));
}

#[test]
fn arrays_enforce_items_lengths_and_structural_uniqueness() {
    let schema = json!({"type":"array", "items":{"type":"integer"}, "minItems":1, "maxItems":3, "uniqueItems":true});
    assert!(validate(&schema, &json!([1, 2, 3])));
    for input in [json!([]), json!([1, 2, 3, 4]), json!([1, "2"]), json!([1, 1.0])] {
        assert!(!validate(&schema, &input), "accepted invalid array {input}");
    }
    assert!(!validate(
        &json!({"uniqueItems":true}),
        &json!([{"x":[1]}, {"x":[1.0]}])
    ));
    assert!(!validate(&json!({"items":false}), &json!([1])));
    assert!(validate(&json!({"items":false}), &json!([])));
}

#[test]
fn combinators_and_boolean_schemas_are_evaluated() {
    let schema = json!({
        "allOf":[{"type":"number"},{"minimum":0}],
        "anyOf":[{"maximum":2},{"minimum":5}],
        "not":{"const":1}
    });
    assert!(validate(&schema, &json!(2)));
    assert!(validate(&schema, &json!(5)));
    for input in [json!(-1), json!(1), json!(3), json!("2")] {
        assert!(!validate(&schema, &input));
    }
    let one = json!({"oneOf":[{"type":"number"},{"type":"integer"}]});
    assert!(validate(&one, &json!(1.5)));
    assert!(!validate(&one, &json!(1)));
    assert!(validate(&json!(true), &json!({})));
    assert!(!validate(&json!(false), &json!({})));
}

#[test]
fn numeric_bounds_do_not_round_large_integers() {
    let schema = json!({"minimum":-1,"exclusiveMaximum":3});
    assert!(validate(&schema, &json!(-1)));
    assert!(!validate(&schema, &json!(-2)));
    assert!(!validate(&schema, &json!(3)));
    assert!(!validate(&json!({"exclusiveMinimum":3}), &json!(3)));
    assert!(!validate(
        &json!({"maximum":9_007_199_254_740_992_u64}),
        &json!(9_007_199_254_740_993_u64)
    ));
    assert!(validate(&json!({"minimum":u64::MAX}), &json!(u64::MAX)));
    assert!(!validate(
        &json!({"not":{"maximum":9_007_199_254_740_992.0}}),
        &json!(9_007_199_254_740_993_u64)
    ));
}

#[test]
fn string_lengths_count_unicode_characters_without_trimming() {
    let schema = json!({"type":"string", "minLength":2, "maxLength":2});
    assert!(validate(&schema, &json!("中😀")));
    assert!(!validate(&schema, &json!("中")));
    assert!(!validate(&schema, &json!(" 中😀")));
}

#[test]
fn portable_patterns_work_and_dialect_ambiguity_fails_closed() {
    assert!(validate(&json!({"pattern":"^[a-z]+", "maxLength":3}), &json!("abc")));
    assert!(!validate(&json!({"pattern":"^[a-z]+"}), &json!("123")));
    assert!(validate(&json!({"pattern":"^\\d+"}), &json!("123")));
    assert!(!validate(&json!({"pattern":"^a.b"}), &json!("a\rb")));
    assert!(validate(&json!({"pattern":"^a.b"}), &json!("a b")));
    assert!(validate(&json!({"pattern":"\\$"}), &json!("$")));
    for pattern in [
        "a$", "(?i)a", "(?=a)a", "(a)\\1", "[a&&b]", "[a--b]", "[\\d-a]", "\\p{L}", "\\s", "[",
    ] {
        assert!(
            !validate(&json!({"anyOf":[true,{"pattern":pattern}]}), &json!("a")),
            "accepted unsupported pattern {pattern}"
        );
    }
    assert!(!validate(&json!({"not":{"pattern":"a$"}}), &json!("a\n")));
    assert!(!validate(&json!({"not":{"pattern":"\\d"}}), &json!("١")));
}

#[test]
fn unsupported_constraints_are_rejected_in_unvisited_branches() {
    for schema in [
        json!({"anyOf":[true,{"$ref":"https://example.invalid/schema"}]}),
        json!({"oneOf":[false,{"format":"email"}]}),
        json!({"properties":{"unused":{"$ref":"file:///secret"}}}),
        json!({"items":{"unknownConstraint":true}}),
        json!({"not":{"properties":{"unused":{"pattern":"(?=x)"}}}}),
        json!({"additionalProperties":{"multipleOf":0.1}}),
        json!({"if":true,"then":false}),
        json!({"$defs":{}}),
    ] {
        assert!(!validate(&schema, &json!({})), "accepted unsupported schema {schema}");
    }
}

#[test]
fn malformed_schema_keyword_shapes_are_rejected() {
    for schema in [
        json!(null),
        json!([]),
        json!("object"),
        json!({"type":[]}),
        json!({"type":["string","string"]}),
        json!({"type":"unknown"}),
        json!({"properties":[]}),
        json!({"required":[1]}),
        json!({"required":["x","x"]}),
        json!({"enum":[]}),
        json!({"allOf":[]}),
        json!({"not":5}),
        json!({"items":[true]}),
        json!({"additionalProperties":3}),
        json!({"minLength":-1}),
        json!({"maxItems":1.5}),
        json!({"uniqueItems":"true"}),
        json!({"exclusiveMinimum":true}),
        json!({"maximum":"4"}),
        json!({"description":{}}),
        json!({"examples":true}),
    ] {
        assert!(!validate(&schema, &json!({})), "accepted malformed schema {schema}");
    }
}

#[test]
fn annotations_are_data_and_only_supported_schema_dialects_are_accepted() {
    let schema = json!({
        "$schema":"https://json-schema.org/draft/2020-12/schema",
        "title":"Tool", "description":"Description",
        "examples":[{"$ref":"literal example"}], "default":{"$ref":"literal default"}
    });
    assert!(validate(&schema, &json!({})));
    assert!(!validate(
        &json!({"$schema":"https://example.invalid/custom-dialect"}),
        &json!({})
    ));
    assert!(!validate(
        &json!({"$schema":"http://json-schema.org/draft-04/schema#"}),
        &json!({})
    ));
}

#[test]
fn sizes_depth_nodes_and_regex_count_are_bounded() {
    assert!(!validate(
        &json!({"description":"x".repeat(MAX_SCHEMA_BYTES)}),
        &Value::Null
    ));
    assert!(!validate(&json!(true), &json!("x".repeat(MAX_INPUT_BYTES))));
    let mut deep = json!(true);
    for _ in 0..=MAX_DEPTH {
        deep = json!({"not":deep});
    }
    assert!(!validate(&deep, &Value::Null));
    let mut input = Value::Null;
    for _ in 0..=MAX_DEPTH {
        input = json!([input]);
    }
    assert!(!validate(&json!(true), &input));
    assert!(!validate(
        &json!({"allOf":vec![json!(true);MAX_SCHEMA_NODES]}),
        &Value::Null
    ));
    assert!(!validate(
        &json!({"allOf":vec![json!({"pattern":"a"});MAX_PATTERNS+1]}),
        &json!("a")
    ));
    assert!(!validate(
        &json!({"pattern":"a".repeat(MAX_PATTERN_BYTES+1)}),
        &json!("a")
    ));
}

#[test]
fn work_exhaustion_propagates_through_negation_instead_of_allowing_input() {
    let source = json!({"not":{"uniqueItems":true}});
    let mut budget = Budget {
        work: 100,
        patterns: MAX_PATTERNS,
    };
    let compiled = Schema::compile(&source, &mut budget, 0).unwrap();
    let input = json!((0..100).collect::<Vec<_>>());
    assert!(compiled.accepts(&input, &mut budget, 0).is_err());
    let source = json!({"anyOf":[true,{"pattern":"a".repeat(1000)}]});
    let input = json!("a".repeat(5000));
    assert!(!validate(&source, &input));
}
