use ropey::Rope;
use tower_lsp::lsp_types::{Position, Range};
use tree_sitter_generate::nativedsl::ast::Span;

/// User-writable DSL type names.
///
/// Mirrors the set the core parser accepts in `parse_type` (`rule_t`, `str_t`,
/// `int_t`, `module_t`, plus the generic `list_t<...>`, `obj_t<...>`, and
/// `tuple_t<...>`). Used by semantic-token classification and completion to
/// flag identifiers that name a DSL type.
pub const DSL_TYPE_NAMES: &[&str] = &[
    "rule_t", "str_t", "int_t", "module_t", "list_t", "obj_t", "tuple_t",
];

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

/// Span of the `index`-th identifier at or after `from`, skipping whitespace
/// and `//` line comments between words.
///
/// `Node::Rule`, `Node::Let`, and `Node::Forward` intern their name as a
/// `StrId` and keep only the *declaration's* span, so the name's own span is
/// not recoverable from the AST. Every one of those forms is
/// `<keyword..> <name>`, and only keywords, whitespace, and comments can
/// precede the name -- so counting identifiers from the declaration start
/// lands on it exactly. Callers pass the number of leading keywords as
/// `index` (`let x` / `expect x` -> 1, `rule x` -> 1, `override rule x` -> 2).
///
/// A leading `r#` is skipped so the returned span covers just the bare name,
/// matching what the lexer spans for a raw identifier.
#[must_use]
pub fn nth_ident_span(source: &str, from: u32, index: usize) -> Option<Span> {
    let bytes = source.as_bytes();
    let is_start = |b: u8| b.is_ascii_alphabetic() || b == b'_';
    let is_continue = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut pos = from as usize;
    let mut seen = 0usize;

    loop {
        // Whitespace and `//` comments can repeat in any order before a word.
        loop {
            while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
                pos += 1;
            }
            if pos + 1 < bytes.len() && bytes[pos] == b'/' && bytes[pos + 1] == b'/' {
                pos += 2;
                while pos < bytes.len() && bytes[pos] != b'\n' {
                    pos += 1;
                }
            } else {
                break;
            }
        }
        if pos >= bytes.len() {
            return None;
        }
        // `r#name`: the lexer spans only the bare name, so start past `r#`.
        if bytes[pos] == b'r'
            && pos + 2 < bytes.len()
            && bytes[pos + 1] == b'#'
            && is_start(bytes[pos + 2])
        {
            pos += 2;
        }
        if !is_start(bytes[pos]) {
            return None;
        }
        let start = pos;
        while pos < bytes.len() && is_continue(bytes[pos]) {
            pos += 1;
        }
        if seen == index {
            return Some(Span::from_usize(start, pos));
        }
        seen += 1;
    }
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
            TokenKind::Ident => {}
            TokenKind::Comma => saw_comma = true,
            TokenKind::LParen => {
                return saw_comma && i > 0 && tokens[i - 1].kind == TokenKind::KwGrammarConfig;
            }
            _ => return false,
        }
    }
    false
}

/// If `offset` lies on the flag-name identifier inside a `#[cfg(NAME)]`
/// attribute, return its span and the name text. Returns `None` for any
/// other token position, including malformed/partial attribute syntax.
#[must_use]
pub fn cfg_flag_at_offset<'src>(
    tokens: &[tree_sitter_generate::nativedsl::lexer::Token],
    source: &'src str,
    offset: u32,
) -> Option<(Span, &'src str)> {
    use tree_sitter_generate::nativedsl::lexer::TokenKind;
    let idx = tokens
        .iter()
        .position(|t| offset >= t.span.start && offset < t.span.end)?;
    let name_tok = tokens.get(idx)?;
    if name_tok.kind != TokenKind::Ident {
        return None;
    }
    // Surrounding pattern: Pound LBracket Ident("cfg") LParen | IDENT | RParen RBracket
    if idx < 4 {
        return None;
    }
    let pound = tokens.get(idx - 4)?;
    let lbracket = tokens.get(idx - 3)?;
    let cfg_ident = tokens.get(idx - 2)?;
    let lparen = tokens.get(idx - 1)?;
    if pound.kind != TokenKind::Pound
        || lbracket.kind != TokenKind::LBracket
        || cfg_ident.kind != TokenKind::Ident
        || &source[cfg_ident.span.start as usize..cfg_ident.span.end as usize] != "cfg"
        || lparen.kind != TokenKind::LParen
    {
        return None;
    }
    Some((
        name_tok.span,
        &source[name_tok.span.start as usize..name_tok.span.end as usize],
    ))
}

/// True if `start_idx` (a token index, exclusive) sits inside the flag-name
/// slot of a `#[cfg(|...)]` attribute: cursor is between the opening `(`
/// and the closing `)`, and the preceding tokens form `#[cfg`.
#[must_use]
pub fn at_cfg_flag_arg(
    tokens: &[tree_sitter_generate::nativedsl::lexer::Token],
    source: &str,
    start_idx: usize,
) -> bool {
    use tree_sitter_generate::nativedsl::lexer::TokenKind;
    let mut i = start_idx;
    while i > 0 {
        i -= 1;
        match tokens[i].kind {
            TokenKind::Ident => {}
            TokenKind::LParen => {
                if i < 3 {
                    return false;
                }
                let pound = &tokens[i - 3];
                let lbracket = &tokens[i - 2];
                let cfg_ident = &tokens[i - 1];
                return pound.kind == TokenKind::Pound
                    && lbracket.kind == TokenKind::LBracket
                    && cfg_ident.kind == TokenKind::Ident
                    && &source[cfg_ident.span.start as usize..cfg_ident.span.end as usize]
                        == "cfg";
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

    // Pin the LSP line-break convention: only LF (and CRLF) terminate a
    // line. Unicode line separators (U+2028, U+2029), NEL, VT, FF, and a
    // lone CR must NOT shift LSP line numbers. Without this, ropey's default
    // unicode-line-break detection would count those chars (typically buried
    // in a regex or string literal) as line breaks, throwing every position
    // mapping after them off by N lines.
    #[test]
    fn unicode_line_separators_are_not_lsp_line_breaks() {
        // Mix VT, FF, U+2028 (LS), U+2029 (PS) into a single line, then
        // newline, then a regular line. Ropey's default features would
        // count these as line breaks; LSP wouldn't.
        let text = "x\u{000B}y\u{000C}z\u{2028}q\u{2029}r\n2\n";
        let rope = Rope::from_str(text);
        // Position (1, 0) should be at the byte after the only real `\n`.
        let line1_offset = text.find('\n').unwrap() as u32 + 1;
        assert_eq!(
            position_to_offset(&rope, Position::new(1, 0)),
            Some(line1_offset)
        );
        // The byte after the only real `\n` (== '2') must report at (1, 0).
        assert_eq!(offset_to_position(&rope, line1_offset), Position::new(1, 0));
        // And the line count itself: 3 ropey "lines" (2 LF + EOF empty).
        assert_eq!(rope.len_lines(), 3);
    }

    #[test]
    fn crlf_counts_as_one_line_break() {
        let text = "a\r\nb\r\n";
        let rope = Rope::from_str(text);
        // 'b' starts at byte 3 (after "a\r\n") and should be at (1, 0).
        let b_offset = text.find('b').unwrap() as u32;
        assert_eq!(offset_to_position(&rope, b_offset), Position::new(1, 0));
        assert_eq!(
            position_to_offset(&rope, Position::new(1, 0)),
            Some(b_offset)
        );
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
        let (documents, id) = crate::analysis::document_map_for_source(
            std::path::Path::new("/tmp/text-test.tsg"),
            source,
        );
        tree_sitter_generate::nativedsl::lexer::Lexer::new(documents.document(id))
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

    /// Resolve `nth_ident_span` to its text so the cases below read as
    /// "which identifier does index N land on".
    fn nth_ident<'s>(source: &'s str, index: usize) -> Option<&'s str> {
        let s = nth_ident_span(source, 0, index)?;
        Some(&source[s.start as usize..s.end as usize])
    }

    /// The declaration forms whose name span the AST no longer carries. The
    /// index is the count of leading keywords, so these pin the contract
    /// `analysis::decl_name_span` relies on.
    #[test]
    fn nth_ident_span_declaration_forms() {
        assert_eq!(nth_ident("rule foo { \"x\" }", 1), Some("foo"));
        assert_eq!(nth_ident("override rule foo { \"x\" }", 2), Some("foo"));
        assert_eq!(nth_ident("let foo = 1", 1), Some("foo"));
        assert_eq!(nth_ident("expect foo", 1), Some("foo"));
    }

    /// A comment may sit between the keyword and the name; it's skipped like
    /// whitespace rather than counted as a word.
    #[test]
    fn nth_ident_span_skips_comments_before_the_name() {
        assert_eq!(nth_ident("rule // note\n foo { \"x\" }", 1), Some("foo"));
        assert_eq!(
            nth_ident("override // a\n rule // b\n foo { \"x\" }", 2),
            Some("foo"),
        );
    }

    /// A raw identifier's span covers only the bare name, matching what the
    /// lexer emits for `r#let` (it advances past `r#` and resets the start).
    #[test]
    fn nth_ident_span_raw_identifier_excludes_prefix() {
        assert_eq!(nth_ident("rule r#let { \"x\" }", 1), Some("let"));
        // A bare `r` is an ordinary identifier, not a raw-ident prefix.
        assert_eq!(nth_ident("rule r { \"x\" }", 1), Some("r"));
    }

    /// Keywords are legal names (`expect_ident_or_kw` accepts them), so the
    /// scan must count words positionally rather than looking for a
    /// non-keyword.
    #[test]
    fn nth_ident_span_keyword_as_name() {
        assert_eq!(nth_ident("rule rule { \"x\" }", 1), Some("rule"));
        assert_eq!(nth_ident("let let = 1", 1), Some("let"));
    }

    /// Running off the end yields `None` so callers fall back to the
    /// declaration span rather than indexing past the buffer.
    #[test]
    fn nth_ident_span_out_of_range() {
        assert_eq!(nth_ident("rule foo { \"x\" }", 9), None);
        assert_eq!(nth_ident("rule", 1), None);
        assert_eq!(nth_ident("", 0), None);
        // Punctuation where a name should be: no identifier to land on.
        assert_eq!(nth_ident("rule { \"x\" }", 1), None);
    }
}
