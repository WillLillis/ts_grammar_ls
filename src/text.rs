use ropey::Rope;
use tower_lsp::lsp_types::{Position, Range};
use tree_sitter_generate::nativedsl::ast::Span;

/// Convert a byte offset to an LSP `Position`.
#[must_use]
pub fn offset_to_position(rope: &Rope, offset: u32) -> Position {
    let offset = (offset as usize).min(rope.len_bytes());
    let char_idx = rope.byte_to_char(offset);
    let line = rope.char_to_line(char_idx);
    let line_start_char = rope.line_to_char(line);
    // LSP uses UTF-16 code units for columns.
    let col = rope.slice(line_start_char..char_idx).len_utf16_cu();
    Position::new(line as u32, col as u32)
}

/// Convert an LSP `Position` to a byte offset.
#[must_use]
pub fn position_to_offset(rope: &Rope, pos: Position) -> Option<u32> {
    let line = pos.line as usize;
    if line >= rope.len_lines() {
        return None;
    }
    let line_start_char = rope.line_to_char(line);
    let line_slice = rope.line(line);

    // Walk UTF-16 code units to find the char index.
    let mut utf16_col = 0usize;
    let mut char_offset = 0usize;
    for ch in line_slice.chars() {
        if utf16_col >= pos.character as usize {
            break;
        }
        utf16_col += ch.len_utf16();
        char_offset += 1;
    }

    let char_idx = line_start_char + char_offset;
    Some(rope.char_to_byte(char_idx) as u32)
}

/// Find the identifier word at a byte offset, returning the slice.
///
/// The cursor is considered "in" a word if the byte at `offset` is an
/// identifier char, OR if the byte at `offset - 1` is (i.e. the cursor sits
/// just past the end of a word, which is a common LSP cursor position).
///
/// Raw identifiers (`r#name`) are recognized: a cursor on the leading `r`,
/// `#`, or anywhere inside the bare-name part returns the bare name (i.e.
/// `r#let` → `"let"`), matching what the lexer / AST extracted.
#[must_use]
pub fn word_at_offset(text: &str, offset: u32) -> Option<&str> {
    let bytes = text.as_bytes();
    let offset = offset as usize;
    let is_ident_byte = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let is_ident_start = |b: u8| b.is_ascii_alphabetic() || b == b'_';

    // Cursor on the `r` of `r#name` (preceded by a non-ident char or BOF).
    if offset < bytes.len()
        && bytes[offset] == b'r'
        && offset + 2 < bytes.len()
        && bytes[offset + 1] == b'#'
        && is_ident_start(bytes[offset + 2])
        && (offset == 0 || !is_ident_byte(bytes[offset - 1]))
    {
        let start = offset + 2;
        let end = text[start..]
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .map_or(text.len(), |i| start + i);
        return Some(&text[start..end]);
    }
    // Cursor on the `#` of `r#name`.
    if offset < bytes.len()
        && bytes[offset] == b'#'
        && offset > 0
        && bytes[offset - 1] == b'r'
        && offset + 1 < bytes.len()
        && is_ident_start(bytes[offset + 1])
        && (offset < 2 || !is_ident_byte(bytes[offset - 2]))
    {
        let start = offset + 1;
        let end = text[start..]
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .map_or(text.len(), |i| start + i);
        return Some(&text[start..end]);
    }

    let probe = if offset < bytes.len() && is_ident_byte(bytes[offset]) {
        offset
    } else if offset > 0 && offset <= bytes.len() && is_ident_byte(bytes[offset - 1]) {
        // Cursor is just past a word; probe the last char of that word.
        offset - 1
    } else {
        return None;
    };

    let start = text[..probe]
        .rfind(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .map_or(0, |i| i + 1);
    let end = text[probe..]
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .map_or(text.len(), |i| probe + i);

    let word = &text[start..end];
    if word.is_empty() { None } else { Some(word) }
}

/// Check if the identifier at `offset` is a grammar config field
/// (inside the grammar block span and followed by `:`).
#[must_use]
pub fn is_grammar_config_field(
    tokens: &[tree_sitter_generate::nativedsl::lexer::Token],
    grammar_span: tree_sitter_generate::nativedsl::ast::Span,
    offset: u32,
) -> bool {
    use tree_sitter_generate::nativedsl::lexer::TokenKind;
    if offset < grammar_span.start || offset >= grammar_span.end {
        return false;
    }
    let Some(idx) = tokens.iter().position(|t| {
        (t.kind == TokenKind::Ident || t.kind.is_keyword())
            && offset >= t.span.start
            && offset < t.span.end
    }) else {
        return false;
    };
    idx + 1 < tokens.len() && tokens[idx + 1].kind == TokenKind::Colon
}

/// Check if the identifier at `offset` is preceded by `::` (i.e., `base::rule_name`).
#[must_use]
pub fn is_base_rule_access(
    tokens: &[tree_sitter_generate::nativedsl::lexer::Token],
    offset: u32,
) -> bool {
    use tree_sitter_generate::nativedsl::lexer::TokenKind;
    let Some(idx) = tokens
        .iter()
        .position(|t| t.kind == TokenKind::Ident && offset >= t.span.start && offset < t.span.end)
    else {
        return false;
    };
    idx > 0 && tokens[idx - 1].kind == TokenKind::ColonColon
}

/// Return the name of the qualifier identifier before `::` at the cursor.
///
/// For `foo::bar`, when the cursor is on `bar`, this returns `"foo"`.
/// Requires `is_base_rule_access` to be true.
#[must_use]
pub fn qualified_access_module<'src>(
    tokens: &[tree_sitter_generate::nativedsl::lexer::Token],
    source: &'src str,
    offset: u32,
) -> Option<&'src str> {
    use tree_sitter_generate::nativedsl::lexer::TokenKind;
    let idx = tokens.iter().position(|t| {
        t.kind == TokenKind::Ident && offset >= t.span.start && offset < t.span.end
    })?;
    // Walk back: expect `::` then an identifier.
    if idx < 2 || tokens[idx - 1].kind != TokenKind::ColonColon {
        return None;
    }
    let qual = &tokens[idx - 2];
    if qual.kind != TokenKind::Ident {
        return None;
    }
    Some(&source[qual.span.start as usize..qual.span.end as usize])
}

/// Check if `start_idx` (a token index, exclusive) sits inside the
/// field-name argument slot of a `grammar_config(module, |...)` call.
/// Walks back through idents/commas/comments to find the opening `(`,
/// requiring at least one comma along the way.
#[must_use]
pub fn at_grammar_config_field_arg(
    tokens: &[tree_sitter_generate::nativedsl::lexer::Token],
    start_idx: usize,
) -> bool {
    use tree_sitter_generate::nativedsl::lexer::TokenKind;

    let mut i = start_idx;
    let mut saw_comma = false;
    while i > 0 {
        i -= 1;
        match tokens[i].kind {
            TokenKind::Ident | TokenKind::Comment => {}
            TokenKind::Comma => saw_comma = true,
            TokenKind::LParen => {
                return saw_comma && i > 0 && tokens[i - 1].kind == TokenKind::KwGrammarConfig;
            }
            _ => return false,
        }
    }
    false
}

#[must_use]
pub fn span_to_range(rope: &Rope, span: Span) -> Range {
    Range::new(
        offset_to_position(rope, span.start),
        offset_to_position(rope, span.end),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_to_position_simple() {
        let text = "hello\nworld\n";
        let rope = Rope::from_str(text);
        assert_eq!(offset_to_position(&rope, 0), Position::new(0, 0));
        assert_eq!(offset_to_position(&rope, 5), Position::new(0, 5));
        assert_eq!(offset_to_position(&rope, 6), Position::new(1, 0));
        assert_eq!(offset_to_position(&rope, 11), Position::new(1, 5));
    }

    #[test]
    fn position_to_offset_simple() {
        let text = "hello\nworld\n";
        let rope = Rope::from_str(text);
        assert_eq!(position_to_offset(&rope, Position::new(0, 0)), Some(0));
        assert_eq!(position_to_offset(&rope, Position::new(0, 5)), Some(5));
        assert_eq!(position_to_offset(&rope, Position::new(1, 0)), Some(6));
        assert_eq!(position_to_offset(&rope, Position::new(1, 5)), Some(11));
    }

    #[test]
    fn position_to_offset_out_of_range() {
        let text = "hello\n";
        let rope = Rope::from_str(text);
        assert_eq!(position_to_offset(&rope, Position::new(99, 0)), None);
    }

    #[test]
    fn word_at_offset_basic() {
        let text = "rule foo_bar { seq(\"a\") }";
        assert_eq!(word_at_offset(text, 0), Some("rule"));
        assert_eq!(word_at_offset(text, 2), Some("rule"));
        assert_eq!(word_at_offset(text, 5), Some("foo_bar"));
        assert_eq!(word_at_offset(text, 9), Some("foo_bar"));
        assert_eq!(word_at_offset(text, 15), Some("seq"));
        // On a non-ident char immediately after a word - returns the preceding word.
        assert_eq!(word_at_offset(text, 4), Some("rule")); // space after "rule"
        assert_eq!(word_at_offset(text, 12), Some("foo_bar")); // space after "foo_bar"
        // On a non-ident char with no preceding ident char.
        assert_eq!(word_at_offset(text, 13), None); // { (preceded by space)
    }

    #[test]
    fn word_at_offset_at_eof() {
        let text = "identifier";
        // Cursor exactly at EOF - should still return the word.
        assert_eq!(word_at_offset(text, 10), Some("identifier"));
        // Cursor past EOF - returns None.
        assert_eq!(word_at_offset(text, 11), None);
    }

    fn lex(source: &str) -> Vec<tree_sitter_generate::nativedsl::lexer::Token> {
        tree_sitter_generate::nativedsl::lexer::Lexer::new(source)
            .tokenize()
            .unwrap()
    }

    #[test]
    fn is_grammar_config_field_inside_block() {
        let source = r#"grammar { language: "test", extras: [" "] }"#;
        let tokens = lex(source);
        // "grammar" spans the whole block.
        let grammar_span = Span::new(0, source.len() as u32);

        // "language" at offset 10 is followed by `:` - it's a config field.
        let lang_offset = source.find("language").unwrap() as u32;
        assert!(is_grammar_config_field(&tokens, grammar_span, lang_offset));

        // "extras" at offset 27 is followed by `:` - it's a config field.
        let extras_offset = source.find("extras").unwrap() as u32;
        assert!(is_grammar_config_field(
            &tokens,
            grammar_span,
            extras_offset
        ));
    }

    #[test]
    fn is_grammar_config_field_outside_block() {
        let source = r#"grammar { language: "test" } rule foo { "x" }"#;
        let tokens = lex(source);
        let grammar_span = Span::new(0, 28);

        // "foo" is outside the grammar block.
        let foo_offset = source.find("foo").unwrap() as u32;
        assert!(!is_grammar_config_field(&tokens, grammar_span, foo_offset));
    }

    #[test]
    fn is_base_rule_access_after_double_colon() {
        let source = r#"grammar { language: "t" } rule foo { base::bar }"#;
        let tokens = lex(source);

        // "bar" is preceded by `::`
        let bar_offset = source.find("bar").unwrap() as u32;
        assert!(is_base_rule_access(&tokens, bar_offset));

        // "base" is NOT preceded by `::`
        let base_offset = source.find("base").unwrap() as u32;
        assert!(!is_base_rule_access(&tokens, base_offset));

        // "foo" is NOT preceded by `::`
        let foo_offset = source.find("foo").unwrap() as u32;
        assert!(!is_base_rule_access(&tokens, foo_offset));
    }

    #[test]
    fn qualified_access_module_returns_qualifier() {
        let source = r#"grammar { language: "t" } rule foo { helpers::func("x") }"#;
        let tokens = lex(source);

        // Cursor on "func" - should return "helpers"
        let func_offset = source.find("func").unwrap() as u32;
        assert_eq!(
            qualified_access_module(&tokens, source, func_offset),
            Some("helpers")
        );

        // Cursor on "helpers" - not preceded by `::`
        let helpers_offset = source.find("helpers").unwrap() as u32;
        assert_eq!(
            qualified_access_module(&tokens, source, helpers_offset),
            None
        );
    }

    #[test]
    fn qualified_access_module_no_qualifier() {
        let source = r#"grammar { language: "t" } rule foo { "x" }"#;
        let tokens = lex(source);

        let foo_offset = source.find("foo").unwrap() as u32;
        assert_eq!(qualified_access_module(&tokens, source, foo_offset), None);
    }
}
