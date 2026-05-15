//! Trivia collection from the lexer token stream.
//!
//! Walks tokens once, attaching line comments and blank-line markers to the
//! non-comment tokens they relate to. Two attachment positions per token:
//!
//! * **Leading** - comments and/or a blank-line marker that appear before
//!   the token, on prior lines. Emitted before the token in the output.
//! * **Trailing** - at most one comment, on the same line as the token,
//!   after it. Emitted at end-of-line after the token.
//!
//! Attachment rule (deterministic):
//!
//! * A comment on the same line as the previous non-comment token, with
//!   no other trivia between, is **trailing** on that token.
//! * Otherwise the comment is **leading** on the next non-comment token.
//! * Blank-line gaps (>=2 newlines) between trivia items or between the
//!   previous token and the next become a `BlankLine` marker in the leading
//!   list of the next token. Consecutive blanks collapse to one marker.
//!
//! Format-off pragma support is layered on top in a later pass.

use std::collections::BTreeMap;

use tree_sitter_generate::nativedsl::lexer::{Token, TokenKind};

/// A piece of leading trivia attached to a non-comment token.
#[derive(Debug, Clone)]
pub enum TriviaItem {
    /// Source text of the comment, including the leading `//`. No trailing
    /// newline.
    Comment(String),
    /// A blank-line gap. Used to preserve paragraph separation between
    /// comments or between a comment block and the token it precedes.
    BlankLine,
}

/// Trivia attached to tokens by source position.
///
/// Keyed by byte offset (the token's `span.start` for leading, `span.end`
/// for trailing) so AST visitors can look up trivia by node span without
/// needing to know token indices.
#[derive(Debug, Default)]
pub struct TriviaMap {
    pub leading: BTreeMap<u32, Vec<TriviaItem>>,
    pub trailing: BTreeMap<u32, String>,
    /// Comments after the last non-comment token, with no following token to
    /// attach them to. Drained by `module()` after the last root item.
    pub tail: Vec<TriviaItem>,
    /// Byte ranges `[off_pragma.start, on_pragma.start)` where the source
    /// should be emitted verbatim. Unclosed ranges extend to EOF.
    pub format_off_ranges: Vec<std::ops::Range<u32>>,
}

impl TriviaMap {
    /// Build the trivia map from a token stream + source text.
    #[must_use]
    pub fn build(tokens: &[Token], source: &str) -> Self {
        let mut map = Self::default();
        // Scan comments for `// tsg-format: off` / `... on` pragmas.
        let mut open_off: Option<u32> = None;
        for tok in tokens {
            if tok.kind != TokenKind::Comment {
                continue;
            }
            let text = source[tok.span.start as usize..tok.span.end as usize]
                .trim_end_matches(['\r', '\n'])
                .trim();
            if text == "// tsg-format: off" {
                if open_off.is_none() {
                    open_off = Some(tok.span.start);
                }
            } else if text == "// tsg-format: on"
                && let Some(start) = open_off.take()
            {
                map.format_off_ranges.push(start..tok.span.start);
            }
        }
        if let Some(start) = open_off {
            map.format_off_ranges
                .push(start..u32::try_from(source.len()).unwrap_or(u32::MAX));
        }
        let mut prev_non_comment_end: Option<u32> = None;
        let mut pending: Vec<TriviaItem> = Vec::new();
        // Position of the last item we already accounted for when checking
        // blank-line gaps. Initially `None`; set to `Some` after first token
        // or first pending comment.
        let mut last_seen_end: Option<u32> = None;

        for tok in tokens {
            match tok.kind {
                TokenKind::Comment => {
                    let text = source[tok.span.start as usize..tok.span.end as usize]
                        .trim_end_matches(['\r', '\n'])
                        .to_owned();
                    let prior_end = last_seen_end.or(prev_non_comment_end).unwrap_or(0);
                    let nls = newlines_between(source, prior_end, tok.span.start);
                    if pending.is_empty()
                        && nls == 0
                        && let Some(end) = prev_non_comment_end
                    {
                        // Trailing on the previous non-comment token.
                        map.trailing.insert(end, text);
                        last_seen_end = Some(tok.span.end);
                        continue;
                    }
                    // Leading on the next non-comment token. Record a
                    // BlankLine marker if there's >=2 newlines since the
                    // last item in this paragraph.
                    if nls >= 2 && !pending.is_empty() {
                        pending.push(TriviaItem::BlankLine);
                    }
                    pending.push(TriviaItem::Comment(text));
                    last_seen_end = Some(tok.span.end);
                }
                TokenKind::Eof => {
                    if !pending.is_empty() {
                        map.tail = std::mem::take(&mut pending);
                    }
                    break;
                }
                _ => {
                    let nls_before = match (last_seen_end, prev_non_comment_end) {
                        (Some(end), _) | (None, Some(end)) => {
                            newlines_between(source, end, tok.span.start)
                        }
                        (None, None) => 0,
                    };
                    if nls_before >= 2 && prev_non_comment_end.is_some() {
                        // Blank line gap before this token. Prepend a marker
                        // to the leading list (still respecting any pending
                        // comments collected so far - they go *after* the
                        // blank-line if the blank is between prev token and
                        // first comment; *before* this token if blank is
                        // between last pending item and this token).
                        if pending.is_empty() {
                            pending.push(TriviaItem::BlankLine);
                        } else {
                            // Blank line between last pending item and the
                            // token itself - already represented implicitly
                            // by the leading list ending and the token
                            // starting on a new line. We push a marker so
                            // the printer can preserve it explicitly.
                            pending.push(TriviaItem::BlankLine);
                        }
                    }
                    if !pending.is_empty() {
                        map.leading
                            .insert(tok.span.start, std::mem::take(&mut pending));
                    }
                    prev_non_comment_end = Some(tok.span.end);
                    last_seen_end = Some(tok.span.end);
                }
            }
        }
        map
    }

    /// Leading trivia attached to the token starting at byte offset `start`.
    #[must_use]
    pub fn leading(&self, start: u32) -> &[TriviaItem] {
        self.leading.get(&start).map_or(&[], Vec::as_slice)
    }

    /// Trailing comment after the token ending at byte offset `end`.
    #[must_use]
    pub fn trailing(&self, end: u32) -> Option<&str> {
        self.trailing.get(&end).map(String::as_str)
    }

    /// First trailing comment whose key falls in `[from, to)`. Used in arg
    /// lists / object entries to find a same-line comment that landed on the
    /// arg itself or on the following separator (typically a comma).
    #[must_use]
    pub fn trailing_in(&self, from: u32, to: u32) -> Option<&str> {
        self.trailing.range(from..to).next().map(|(_, s)| s.as_str())
    }
}

fn newlines_between(source: &str, from: u32, to: u32) -> usize {
    source[from as usize..to as usize]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tree_sitter_generate::nativedsl::lexer::Lexer;

    fn tokenize(src: &str) -> Vec<Token> {
        Lexer::new(src).tokenize().unwrap()
    }

    #[test]
    fn trailing_comment_after_token() {
        let src = "rule x { \"y\" } // trailing\n";
        let tokens = tokenize(src);
        let map = TriviaMap::build(&tokens, src);
        // Find the `}` token.
        let rbrace = tokens
            .iter()
            .find(|t| t.kind == TokenKind::RBrace)
            .unwrap();
        assert_eq!(map.trailing(rbrace.span.end), Some("// trailing"));
    }

    #[test]
    fn leading_comment_before_token() {
        let src = "// docs\nrule x { \"y\" }\n";
        let tokens = tokenize(src);
        let map = TriviaMap::build(&tokens, src);
        let rule_kw = tokens
            .iter()
            .find(|t| t.kind == TokenKind::KwRule)
            .unwrap();
        let leading = map.leading(rule_kw.span.start);
        assert_eq!(leading.len(), 1);
        assert!(matches!(&leading[0], TriviaItem::Comment(s) if s == "// docs"));
    }

    #[test]
    fn stacked_leading_comments() {
        let src = "// a\n// b\n// c\nrule x { \"y\" }\n";
        let tokens = tokenize(src);
        let map = TriviaMap::build(&tokens, src);
        let rule_kw = tokens
            .iter()
            .find(|t| t.kind == TokenKind::KwRule)
            .unwrap();
        let leading = map.leading(rule_kw.span.start);
        let texts: Vec<&str> = leading
            .iter()
            .filter_map(|t| match t {
                TriviaItem::Comment(s) => Some(s.as_str()),
                TriviaItem::BlankLine => None,
            })
            .collect();
        assert_eq!(texts, vec!["// a", "// b", "// c"]);
        // No blank-line markers between them - they're stacked.
        assert!(
            !leading
                .iter()
                .any(|t| matches!(t, TriviaItem::BlankLine))
        );
    }

    #[test]
    fn blank_line_between_comment_groups() {
        let src = "// a\n// b\n\n// c\nrule x { \"y\" }\n";
        let tokens = tokenize(src);
        let map = TriviaMap::build(&tokens, src);
        let rule_kw = tokens
            .iter()
            .find(|t| t.kind == TokenKind::KwRule)
            .unwrap();
        let leading = map.leading(rule_kw.span.start);
        // Expect: Comment("// a"), Comment("// b"), BlankLine, Comment("// c").
        assert_eq!(leading.len(), 4);
        assert!(matches!(&leading[0], TriviaItem::Comment(s) if s == "// a"));
        assert!(matches!(&leading[1], TriviaItem::Comment(s) if s == "// b"));
        assert!(matches!(&leading[2], TriviaItem::BlankLine));
        assert!(matches!(&leading[3], TriviaItem::Comment(s) if s == "// c"));
    }

    #[test]
    fn no_trivia_means_empty_leading() {
        let src = "rule x { \"y\" }\n";
        let tokens = tokenize(src);
        let map = TriviaMap::build(&tokens, src);
        let rule_kw = tokens
            .iter()
            .find(|t| t.kind == TokenKind::KwRule)
            .unwrap();
        assert!(map.leading(rule_kw.span.start).is_empty());
    }
}
