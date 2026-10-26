//! Deliberately limited, fail-closed JSON Schema checking for DSML recovery.
//!
//! This is not a general JSON Schema implementation. Every schema branch is
//! checked before evaluation, including branches that the input will not visit.
//! Unsupported schemas leave DSML as text; they must never authorize execution.
//! References are never resolved and this module performs no network or file IO.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use regex::{Regex, RegexBuilder};
use serde_json::{Map, Number, Value};

const MAX_SCHEMA_BYTES: usize = 64 * 1024;
const MAX_INPUT_BYTES: usize = 1024 * 1024;
const MAX_DEPTH: usize = 48;
const MAX_SCHEMA_NODES: usize = 4096;
const MAX_INPUT_NODES: usize = 65_536;
const MAX_WORK: usize = 4 * 1024 * 1024;
const MAX_PATTERNS: usize = 32;
const MAX_PATTERN_BYTES: usize = 2048;

type Check<T> = Result<T, ()>;

pub(super) fn validate(schema: &Value, input: &Value) -> bool {
    let run = || -> Check<bool> {
        check_size(schema, MAX_SCHEMA_BYTES, MAX_SCHEMA_NODES)?;
        check_size(input, MAX_INPUT_BYTES, MAX_INPUT_NODES)?;
        let mut budget = Budget {
            work: MAX_WORK,
            patterns: MAX_PATTERNS,
        };
        let schema = Schema::compile(schema, &mut budget, 0)?;
        schema.accepts(input, &mut budget, 0)
    };
    matches!(run(), Ok(true))
}

struct Budget {
    work: usize,
    patterns: usize,
}

impl Budget {
    fn spend(&mut self, amount: usize) -> Check<()> {
        self.work = self.work.checked_sub(amount).ok_or(())?;
        Ok(())
    }
}

// Traverse without serializing/cloning an untrusted value or recursing deeply.
// String accounting is a conservative bound on its JSON-encoded byte length.
fn check_size(value: &Value, max_bytes: usize, max_nodes: usize) -> Check<()> {
    fn visit(value: &Value, bytes: &mut usize, nodes: &mut usize, depth: usize) -> Check<()> {
        if depth > MAX_DEPTH {
            return Err(());
        }
        *nodes = nodes.checked_sub(1).ok_or(())?;
        *bytes = bytes.checked_sub(2).ok_or(())?;
        match value {
            Value::String(text) => string_size(text, bytes)?,
            Value::Array(values) => {
                for value in values {
                    *bytes = bytes.checked_sub(1).ok_or(())?;
                    visit(value, bytes, nodes, depth + 1)?;
                }
            }
            Value::Object(values) => {
                for (name, value) in values {
                    *bytes = bytes.checked_sub(4).ok_or(())?;
                    string_size(name, bytes)?;
                    visit(value, bytes, nodes, depth + 1)?;
                }
            }
            // The numeric representation is bounded by serde_json's Number.
            Value::Number(_) => *bytes = bytes.checked_sub(24).ok_or(())?,
            Value::Bool(_) | Value::Null => *bytes = bytes.checked_sub(5).ok_or(())?,
        }
        Ok(())
    }
    fn string_size(text: &str, bytes: &mut usize) -> Check<()> {
        *bytes = bytes.checked_sub(text.len()).ok_or(())?;
        for byte in text.bytes() {
            let extra = match byte {
                0..=31 => 5,
                b'"' | b'\\' => 1,
                _ => 0,
            };
            *bytes = bytes.checked_sub(extra).ok_or(())?;
        }
        Ok(())
    }
    let mut bytes = max_bytes;
    let mut nodes = max_nodes;
    visit(value, &mut bytes, &mut nodes, 0)
}

struct Schema<'a> {
    source: &'a Value,
    properties: BTreeMap<&'a str, Schema<'a>>,
    additional: Option<Box<Schema<'a>>>,
    items: Option<Box<Schema<'a>>>,
    all_of: Vec<Schema<'a>>,
    any_of: Vec<Schema<'a>>,
    one_of: Vec<Schema<'a>>,
    not: Option<Box<Schema<'a>>>,
    pattern: Option<Regex>,
}

impl<'a> Schema<'a> {
    fn compile(source: &'a Value, budget: &mut Budget, depth: usize) -> Check<Self> {
        budget.spend(1)?;
        if depth > MAX_DEPTH {
            return Err(());
        }
        let mut compiled = Self {
            source,
            properties: BTreeMap::new(),
            additional: None,
            items: None,
            all_of: Vec::new(),
            any_of: Vec::new(),
            one_of: Vec::new(),
            not: None,
            pattern: None,
        };
        if source.is_boolean() {
            return Ok(compiled);
        }
        let map = source.as_object().ok_or(())?;
        for (key, value) in map {
            budget.spend(1)?;
            match key.as_str() {
                // Annotation values are checked for their documented shapes,
                // but never affect whether an input is accepted.
                "title" | "description" => {
                    value.as_str().ok_or(())?;
                }
                "$schema" => {
                    if !matches!(
                        value.as_str(),
                        Some("http://json-schema.org/draft-07/schema#")
                            | Some("https://json-schema.org/draft-07/schema#")
                            | Some("https://json-schema.org/draft/2019-09/schema")
                            | Some("https://json-schema.org/draft/2020-12/schema")
                    ) {
                        return Err(());
                    }
                }
                "default" => {}
                "examples" => {
                    value.as_array().ok_or(())?;
                }
                "type" => validate_types(value)?,
                "properties" => {
                    for (name, schema) in value.as_object().ok_or(())? {
                        compiled
                            .properties
                            .insert(name, Self::compile(schema, budget, depth + 1)?);
                    }
                }
                "additionalProperties" => {
                    compiled.additional = Some(Box::new(Self::compile(value, budget, depth + 1)?));
                }
                "items" => compiled.items = Some(Box::new(Self::compile(value, budget, depth + 1)?)),
                "required" => {
                    let names = value.as_array().ok_or(())?;
                    for (index, name) in names.iter().enumerate() {
                        let name = name.as_str().ok_or(())?;
                        for previous in &names[..index] {
                            budget.spend(name.len() + 1)?;
                            if previous.as_str() == Some(name) {
                                return Err(());
                            }
                        }
                    }
                }
                "enum" => {
                    let values = value.as_array().filter(|values| !values.is_empty()).ok_or(())?;
                    for (index, value) in values.iter().enumerate() {
                        for previous in &values[..index] {
                            if equal(value, previous, budget, 0)? {
                                return Err(());
                            }
                        }
                    }
                }
                "const" => {}
                "allOf" | "anyOf" | "oneOf" => {
                    let mut children = Vec::new();
                    for child in value.as_array().filter(|values| !values.is_empty()).ok_or(())? {
                        children.push(Self::compile(child, budget, depth + 1)?);
                    }
                    match key.as_str() {
                        "allOf" => compiled.all_of = children,
                        "anyOf" => compiled.any_of = children,
                        _ => compiled.one_of = children,
                    }
                }
                "not" => compiled.not = Some(Box::new(Self::compile(value, budget, depth + 1)?)),
                "minLength" | "maxLength" | "minItems" | "maxItems" | "minProperties" | "maxProperties" => {
                    value.as_u64().ok_or(())?;
                }
                "uniqueItems" => {
                    value.as_bool().ok_or(())?;
                }
                "minimum" | "maximum" | "exclusiveMinimum" | "exclusiveMaximum" => {
                    value.as_number().ok_or(())?;
                }
                "pattern" => {
                    budget.patterns = budget.patterns.checked_sub(1).ok_or(())?;
                    let pattern = portable_pattern(value.as_str().ok_or(())?)?;
                    budget.spend(pattern.len())?;
                    compiled.pattern = Some(
                        RegexBuilder::new(&pattern)
                            .size_limit(64 * 1024)
                            .dfa_size_limit(64 * 1024)
                            .nest_limit(32)
                            .build()
                            .map_err(|_| ())?,
                    );
                }
                // In particular reject $ref/$defs, format, conditional schemas,
                // tuple items and newer unevaluated/dependency constraints.
                _ => return Err(()),
            }
        }
        Ok(compiled)
    }

    fn accepts(&self, input: &Value, budget: &mut Budget, depth: usize) -> Check<bool> {
        budget.spend(1)?;
        if depth > MAX_DEPTH {
            return Err(());
        }
        if let Some(allowed) = self.source.as_bool() {
            return Ok(allowed);
        }
        let map = self.source.as_object().ok_or(())?;
        if let Some(types) = map.get("type") {
            let allowed = match types {
                Value::String(kind) => matches_type(kind, input),
                Value::Array(types) => types
                    .iter()
                    .any(|kind| matches_type(kind.as_str().unwrap_or(""), input)),
                _ => false,
            };
            if !allowed {
                return Ok(false);
            }
        }
        if let Some(expected) = map.get("const")
            && !equal(expected, input, budget, 0)?
        {
            return Ok(false);
        }
        if let Some(values) = map.get("enum").and_then(Value::as_array) {
            let mut found = false;
            for value in values {
                if equal(value, input, budget, 0)? {
                    found = true;
                    break;
                }
            }
            if !found {
                return Ok(false);
            }
        }
        for child in &self.all_of {
            if !child.accepts(input, budget, depth + 1)? {
                return Ok(false);
            }
        }
        for (children, exactly_one) in [(&self.any_of, false), (&self.one_of, true)] {
            if children.is_empty() {
                continue;
            }
            let mut count = 0;
            for child in children {
                if child.accepts(input, budget, depth + 1)? {
                    count += 1;
                }
            }
            if count == 0 || (exactly_one && count != 1) {
                return Ok(false);
            }
        }
        if let Some(child) = &self.not
            && child.accepts(input, budget, depth + 1)?
        {
            return Ok(false);
        }
        match input {
            Value::Object(values) => {
                if !within_lengths(map, "minProperties", "maxProperties", values.len()) {
                    return Ok(false);
                }
                if let Some(required) = map.get("required").and_then(Value::as_array) {
                    for name in required {
                        let name = name.as_str().ok_or(())?;
                        budget.spend(name.len() + 1)?;
                        if !values.contains_key(name) {
                            return Ok(false);
                        }
                    }
                }
                for (name, value) in values {
                    budget.spend(name.len() + 1)?;
                    let schema = self.properties.get(name.as_str()).or(self.additional.as_deref());
                    if let Some(schema) = schema
                        && !schema.accepts(value, budget, depth + 1)?
                    {
                        return Ok(false);
                    }
                }
            }
            Value::Array(values) => {
                if !within_lengths(map, "minItems", "maxItems", values.len()) {
                    return Ok(false);
                }
                for (index, value) in values.iter().enumerate() {
                    budget.spend(1)?;
                    if let Some(schema) = &self.items
                        && !schema.accepts(value, budget, depth + 1)?
                    {
                        return Ok(false);
                    }
                    if map.get("uniqueItems").and_then(Value::as_bool) == Some(true) {
                        for previous in &values[..index] {
                            if equal(value, previous, budget, 0)? {
                                return Ok(false);
                            }
                        }
                    }
                }
            }
            Value::String(text) => {
                if map.contains_key("minLength") || map.contains_key("maxLength") {
                    budget.spend(text.len())?;
                    if !within_lengths(map, "minLength", "maxLength", text.chars().count()) {
                        return Ok(false);
                    }
                }
                if let Some(pattern) = &self.pattern {
                    // Avoid Unicode/UTF-16 dialect differences. This is an
                    // indeterminate result, not a failed match: `not` must not
                    // turn an unsupported comparison into permission to run.
                    if !text.is_ascii() {
                        return Err(());
                    }
                    // Rust regex is linear in input length with a pattern-size
                    // factor. Charge both, including repeated array/branch use.
                    budget.spend(
                        text.len()
                            .saturating_add(1)
                            .saturating_mul(pattern.as_str().len().max(1)),
                    )?;
                    if !pattern.is_match(text) {
                        return Ok(false);
                    }
                }
            }
            Value::Number(number) => {
                for (keyword, forbidden) in [
                    ("minimum", Ordering::Less),
                    ("maximum", Ordering::Greater),
                    ("exclusiveMinimum", Ordering::Less),
                    ("exclusiveMaximum", Ordering::Greater),
                ] {
                    if let Some(bound) = map.get(keyword).and_then(Value::as_number) {
                        let ordering = compare_numbers(number, bound)?;
                        if ordering == forbidden || (keyword.starts_with("exclusive") && ordering == Ordering::Equal) {
                            return Ok(false);
                        }
                    }
                }
            }
            Value::Null | Value::Bool(_) => {}
        }
        Ok(true)
    }
}

fn validate_types(value: &Value) -> Check<()> {
    fn supported(name: &str) -> bool {
        matches!(
            name,
            "null" | "boolean" | "object" | "array" | "number" | "integer" | "string"
        )
    }
    match value {
        Value::String(name) if supported(name) => Ok(()),
        Value::Array(names) if !names.is_empty() => {
            for (index, name) in names.iter().enumerate() {
                if !name.as_str().is_some_and(supported) || names[..index].contains(name) {
                    return Err(());
                }
            }
            Ok(())
        }
        _ => Err(()),
    }
}

fn matches_type(kind: &str, value: &Value) -> bool {
    match kind {
        "null" => value.is_null(),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "number" => value.is_number(),
        "integer" => value.as_number().is_some_and(|number| {
            number.is_i64() || number.is_u64() || number.as_f64().is_some_and(|number| number.fract() == 0.0)
        }),
        "string" => value.is_string(),
        _ => false,
    }
}

fn within_lengths(map: &Map<String, Value>, min: &str, max: &str, length: usize) -> bool {
    let length = length as u64;
    map.get(min)
        .and_then(Value::as_u64)
        .is_none_or(|minimum| length >= minimum)
        && map
            .get(max)
            .and_then(Value::as_u64)
            .is_none_or(|maximum| length <= maximum)
}

fn compare_numbers(left: &Number, right: &Number) -> Check<Ordering> {
    fn integer(value: &Number) -> Option<i128> {
        value
            .as_i64()
            .map(i128::from)
            .or_else(|| value.as_u64().map(i128::from))
    }
    if let (Some(left), Some(right)) = (integer(left), integer(right)) {
        return Ok(left.cmp(&right));
    }
    // Never silently round a large integer when comparing it with a float.
    // Uncertain numeric comparisons reject recovery, including under `not`.
    for value in [left, right] {
        if integer(value).is_some_and(|integer| integer.abs() > (1_i128 << 53)) {
            return Err(());
        }
    }
    left.as_f64()
        .and_then(|left| right.as_f64().and_then(|right| left.partial_cmp(&right)))
        .ok_or(())
}

fn equal(left: &Value, right: &Value, budget: &mut Budget, depth: usize) -> Check<bool> {
    budget.spend(1)?;
    if depth > MAX_DEPTH {
        return Err(());
    }
    Ok(match (left, right) {
        (Value::Null, Value::Null) => true,
        (Value::Bool(left), Value::Bool(right)) => left == right,
        (Value::Number(left), Value::Number(right)) => compare_numbers(left, right)? == Ordering::Equal,
        (Value::String(left), Value::String(right)) => {
            budget.spend(left.len().min(right.len()))?;
            left == right
        }
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                return Ok(false);
            }
            for (left, right) in left.iter().zip(right) {
                if !equal(left, right, budget, depth + 1)? {
                    return Ok(false);
                }
            }
            true
        }
        (Value::Object(left), Value::Object(right)) => {
            if left.len() != right.len() {
                return Ok(false);
            }
            for (name, left) in left {
                budget.spend(name.len() + 1)?;
                let Some(right) = right.get(name) else {
                    return Ok(false);
                };
                if !equal(left, right, budget, depth + 1)? {
                    return Ok(false);
                }
            }
            true
        }
        _ => false,
    })
}

// Only a portable subset of ECMAScript patterns is supported. Rust-specific
// flags, class operations, lookaround, backreferences and Unicode escapes fail
// closed. ASCII digit/word escapes are translated explicitly; Rust's Unicode
// \d / \w would otherwise accept inputs an ECMAScript schema forbids. Dot also
// excludes ECMAScript line terminators. End anchors are unsupported because the
// dialects differ before a final newline; even stricter matches are unsafe under
// `not`/`oneOf`. Patterns and their evaluated strings must be ASCII.
fn portable_pattern(pattern: &str) -> Check<String> {
    if pattern.len() > MAX_PATTERN_BYTES || !pattern.is_ascii() {
        return Err(());
    }
    let mut output = String::new();
    let mut chars = pattern.chars().peekable();
    let mut in_class = false;
    while let Some(ch) = chars.next() {
        match ch {
            '\\' => {
                let escaped = chars.next().ok_or(())?;
                match escaped {
                    'd' if !in_class => output.push_str("[0-9]"),
                    'w' if !in_class => output.push_str("[A-Za-z0-9_]"),
                    'D' if !in_class => output.push_str("[^0-9]"),
                    'W' if !in_class => output.push_str("[^A-Za-z0-9_]"),
                    '.' | '^' | '$' | '*' | '+' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' | '/' | '-'
                    | 'n' | 'r' | 't' | 'f' => {
                        output.push('\\');
                        output.push(escaped);
                    }
                    _ => return Err(()),
                }
            }
            '[' if !in_class => {
                in_class = true;
                output.push(ch);
            }
            '[' => return Err(()),
            ']' if in_class => {
                in_class = false;
                output.push(ch);
            }
            ']' => return Err(()),
            '$' if !in_class => return Err(()),
            '&' | '~' if in_class => return Err(()),
            '-' if in_class && chars.peek() == Some(&'-') => return Err(()),
            '(' if !in_class && chars.peek() == Some(&'?') => {
                chars.next();
                if chars.next() != Some(':') {
                    return Err(());
                }
                output.push_str("(?:");
            }
            '.' if !in_class => output.push_str("[^\\n\\r\\x{2028}\\x{2029}]"),
            _ => output.push(ch),
        }
    }
    if in_class {
        return Err(());
    }
    Ok(output)
}

#[cfg(test)]
#[path = "schema_tests.rs"]
mod tests;
