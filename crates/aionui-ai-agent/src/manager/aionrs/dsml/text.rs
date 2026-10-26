use super::parser::{MAX_BYTES, OPEN};

#[derive(Default)]
enum Line {
    #[default]
    Start,
    Probe,
    Marker(char, usize),
    Closing,
    Rest,
}

/// A streaming, bounded line lexer. Only a calls marker at a standalone line's
/// beginning (at most three spaces) can become executable protocol. This avoids
/// interpreting inline examples, blockquotes and Markdown code as tool requests.
#[derive(Default)]
pub(super) struct TextBuffer {
    line: Line,
    fence: Option<(char, usize)>,
    inline: Option<usize>,
    backtick_run: usize,
    escaped: bool,
    quoted: bool,
    line_has_content: bool,
    pending: String,
    candidate: Option<String>,
    disabled: bool,
}

impl TextBuffer {
    pub(super) fn push(&mut self, delta: &str) -> String {
        if self.disabled {
            return delta.to_owned();
        }
        let mut plain = String::new();
        for ch in delta.chars() {
            if let Some(candidate) = &mut self.candidate {
                if candidate.len() + ch.len_utf8() <= MAX_BYTES {
                    candidate.push(ch);
                } else {
                    plain.push_str(&self.disable());
                    plain.push(ch);
                    tracing::warn!(reason = "byte_limit", "DSML compatibility buffer limit reached");
                }
                continue;
            }
            if self.disabled {
                plain.push(ch);
                continue;
            }
            if !is_separator(ch) {
                self.line_has_content = true;
            }
            match self.line {
                Line::Start => {
                    if ch == ' ' && self.pending.len() < 3 {
                        self.pending.push(ch);
                        continue;
                    }
                    if self.fence.is_none() && self.inline.is_none() && !self.quoted && ch == '<' {
                        plain.push_str(&std::mem::take(&mut self.pending));
                        self.pending.push(ch);
                        self.line = Line::Probe;
                        continue;
                    }
                    plain.push_str(&std::mem::take(&mut self.pending));
                    if self.fence.is_none() && self.inline.is_none() && ch == '>' {
                        // A blockquote paragraph may continue lazily without a `>`
                        // on every line. Keep it quoted until a blank line.
                        self.quoted = true;
                    }
                    self.line = if matches!(ch, '`' | '~') && self.inline.is_none() && !self.quoted {
                        Line::Marker(ch, 1)
                    } else {
                        Line::Rest
                    };
                    plain.push(ch);
                    if matches!(self.line, Line::Rest) {
                        self.inline_character(ch);
                    }
                }
                Line::Probe => {
                    self.pending.push(ch);
                    if self.pending == OPEN {
                        self.candidate = Some(std::mem::take(&mut self.pending));
                    } else if !OPEN.starts_with(&self.pending) {
                        plain.push_str(&std::mem::take(&mut self.pending));
                        self.line = Line::Rest;
                        self.inline_character(ch);
                    }
                }
                Line::Marker(marker, count) => {
                    plain.push(ch);
                    if ch == marker {
                        self.line = Line::Marker(marker, count.saturating_add(1));
                    } else {
                        if let Some((active, length)) = self.fence {
                            self.line = if marker == active && count >= length && is_separator(ch) {
                                Line::Closing
                            } else {
                                Line::Rest
                            };
                        } else {
                            if count >= 3 {
                                self.fence = Some((marker, count));
                            } else {
                                if marker == '`' {
                                    self.inline = Some(count);
                                }
                                self.inline_character(ch);
                            }
                            self.line = Line::Rest;
                        }
                    }
                }
                Line::Closing => {
                    plain.push(ch);
                    if !is_separator(ch) {
                        self.line = Line::Rest;
                    }
                }
                Line::Rest => {
                    plain.push(ch);
                    self.inline_character(ch);
                }
            }
            if ch == '\n' {
                if matches!(self.line, Line::Closing) {
                    self.fence = None;
                }
                if !self.line_has_content {
                    self.quoted = false;
                }
                self.line_has_content = false;
                self.line = Line::Start;
            }
        }
        plain
    }

    /// Track inline code with constant space, including delimiter runs split
    /// across deltas and code spans crossing line boundaries. Unclosed spans
    /// conservatively suppress recovery for the remainder of the response.
    fn inline_character(&mut self, ch: char) {
        if self.fence.is_some() || self.quoted {
            return;
        }
        if ch == '`' && (self.inline.is_some() || !self.escaped) {
            self.backtick_run = self.backtick_run.saturating_add(1);
            return;
        }
        if self.backtick_run != 0 {
            if self.inline == Some(self.backtick_run) {
                self.inline = None;
            } else if self.inline.is_none() {
                self.inline = Some(self.backtick_run);
            }
            self.backtick_run = 0;
        }
        self.escaped = self.inline.is_none() && ch == '\\' && !self.escaped;
    }

    pub(super) fn disable(&mut self) -> String {
        self.disabled = true;
        self.candidate
            .take()
            .unwrap_or_else(|| std::mem::take(&mut self.pending))
    }

    pub(super) fn finish(mut self) -> String {
        self.disable()
    }
}

fn is_separator(ch: char) -> bool {
    matches!(ch, ' ' | '\t' | '\r' | '\n')
}

#[cfg(test)]
#[path = "text_tests.rs"]
mod tests;
