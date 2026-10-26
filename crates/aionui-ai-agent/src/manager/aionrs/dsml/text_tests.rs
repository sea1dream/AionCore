use super::*;

const BLOCK: &str = "<｜DSML｜ calls>\n<｜DSML｜ invoke name=\"Check\"></｜DSML｜ invoke>\n</｜DSML｜ calls>";

fn assert_buffered(source: &str, expected_plain: &str, expected_candidate: &str) {
    for split in source.char_indices().map(|(offset, _)| offset).chain([source.len()]) {
        let mut buffer = TextBuffer::default();
        let plain = buffer.push(&source[..split]) + &buffer.push(&source[split..]);
        assert_eq!(plain, expected_plain, "plain text at split {split}");
        assert_eq!(buffer.finish(), expected_candidate, "candidate at split {split}");
    }
    let mut buffer = TextBuffer::default();
    let plain: String = source.chars().map(|ch| buffer.push(&ch.to_string())).collect();
    assert_eq!(plain, expected_plain, "one character per delta");
    assert_eq!(buffer.finish(), expected_candidate, "one character per delta");
}

#[test]
fn multiline_inline_code_never_becomes_a_candidate() {
    for prefix in [
        "`\n",
        "``\n",
        "Example: `code\n",
        "Example: ``code\n",
        "Example: ```code\n",
    ] {
        let source = format!("{prefix}{BLOCK}");
        assert_buffered(&source, &source, "");
    }
    let source = format!("Example: `code\n{BLOCK}\n`");
    assert_buffered(&source, &source, "");
}

#[test]
fn inline_code_requires_a_matching_backtick_run_length() {
    for middle in ["`", "```", "````"] {
        let source = format!("Example: ``code\n{middle}\n{BLOCK}");
        assert_buffered(&source, &source, "");
    }
}

#[test]
fn properly_closed_multiline_inline_code_allows_a_later_call() {
    for delimiter in ["`", "``", "```", "````"] {
        let prefix = format!("Example: {delimiter}code\ncontinued{delimiter}\n");
        assert_buffered(&format!("{prefix}{BLOCK}"), &prefix, BLOCK);
    }
    let prefix = "Example: ```code\n```\n";
    assert_buffered(&format!("{prefix}{BLOCK}"), prefix, BLOCK);
}

#[test]
fn escaped_backticks_and_backslash_parity_follow_inline_code_context() {
    for prefix in [
        "Example: \\`literal\n",
        "Example: \\`\\`literal\n",
        "Example: `code\\`\n",
    ] {
        assert_buffered(&format!("{prefix}{BLOCK}"), prefix, BLOCK);
    }
    let source = format!("Example: \\\\`code\n{BLOCK}");
    assert_buffered(&source, &source, "");
}

#[test]
fn blockquotes_and_lazy_continuations_stay_plain_until_a_blank_line() {
    for prefix in ["> ", "   > ", "> explanation\n", "> > nested\n"] {
        let source = format!("{prefix}{BLOCK}");
        assert_buffered(&source, &source, "");
    }
    for prefix in [
        "> explanation\n\n",
        "> explanation\n \t\r\n",
        "> example\n> continuation\n\n",
    ] {
        assert_buffered(&format!("{prefix}{BLOCK}"), prefix, BLOCK);
    }
}

#[test]
fn indented_inline_and_escaped_markers_are_not_candidates() {
    for prefix in ["    ", "\t", "   \t", "\\", "Example: ", "  Example: "] {
        let source = format!("{prefix}{BLOCK}");
        assert_buffered(&source, &source, "");
    }
    for indent in ["", " ", "  ", "   "] {
        assert_buffered(&format!("{indent}{BLOCK}"), indent, BLOCK);
    }
}

#[test]
fn fence_character_length_and_closing_suffix_must_match() {
    for (opening, closing) in [
        ("```", "``"),
        ("````", "```"),
        ("~~~~", "~~~"),
        ("```", "~~~"),
        ("~~~", "```"),
        ("```", "``` trailing"),
        ("~~~", "~~~ trailing"),
        ("```", "```\u{000b}"),
        ("```", "```\u{000c}"),
        ("```", "    ```"),
    ] {
        let source = format!("{opening}\nexample\n{closing}\n{BLOCK}");
        assert_buffered(&source, &source, "");
    }
}

#[test]
fn correctly_closed_fences_allow_a_later_standalone_call() {
    for (opening, closing) in [
        ("```", "```"),
        ("```rust", "````"),
        ("~~~", "~~~~"),
        ("  ````xml", "   ````\t "),
        ("   ~~~~xml", " ~~~~~ \t"),
    ] {
        for newline in ["\n", "\r\n"] {
            let prefix = format!("{opening}{newline}example{newline}{closing}{newline}");
            assert_buffered(&format!("{prefix}{BLOCK}"), &prefix, BLOCK);
        }
    }
}

#[test]
fn fenced_examples_remain_plain_even_with_inline_backticks_inside() {
    for (opening, body) in [("```", "`unfinished"), ("~~~", "``unfinished")] {
        let source = format!("{opening}\n{body}\n{BLOCK}\n{opening}");
        assert_buffered(&source, &source, "");
        let prefix = format!("{source}\n");
        assert_buffered(&format!("{prefix}{BLOCK}"), &prefix, BLOCK);
    }
}

#[test]
fn failed_marker_probes_preserve_text_and_track_following_inline_code() {
    for source in ["<not dsml", "中文\n<｜D", "   <different>\nanswer"] {
        let mut buffer = TextBuffer::default();
        let plain = buffer.push(source);
        assert_eq!(plain + &buffer.finish(), source);
    }
    let source = format!("<`inline\n{BLOCK}");
    assert_buffered(&source, &source, "");
}

#[test]
fn disabling_flushes_only_buffered_text_and_prevents_later_candidates() {
    let mut buffer = TextBuffer::default();
    assert_eq!(buffer.push("prefix\n<｜D"), "prefix\n");
    assert_eq!(buffer.disable(), "<｜D");
    assert_eq!(buffer.push(BLOCK), BLOCK);
    assert!(buffer.finish().is_empty());

    let mut buffer = TextBuffer::default();
    assert!(buffer.push(BLOCK).is_empty());
    assert_eq!(buffer.disable(), BLOCK);
    assert!(buffer.finish().is_empty());
}

#[test]
fn oversized_candidates_flush_losslessly_and_disable_recovery() {
    let source = format!("{OPEN}{}", "x".repeat(MAX_BYTES));
    let mut buffer = TextBuffer::default();
    assert_eq!(buffer.push(&source), source);
    assert!(buffer.candidate.is_none());
    assert!(buffer.pending.is_empty());
    assert!(buffer.disabled);
    assert_eq!(buffer.push(BLOCK), BLOCK);
    assert!(buffer.finish().is_empty());
}

#[test]
fn very_long_backtick_runs_use_constant_state_and_do_not_wrap() {
    let source = format!("Example: {}code\n{BLOCK}", "`".repeat(100_000));
    let mut buffer = TextBuffer::default();
    assert_eq!(buffer.push(&source), source);
    assert_eq!(buffer.inline, Some(100_000));
    assert!(buffer.pending.is_empty());
    assert!(buffer.candidate.is_none());
    assert!(buffer.finish().is_empty());
}
