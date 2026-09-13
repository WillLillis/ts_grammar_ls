//! Reusable source-edit primitives for lint fixes (and, later, refactors).
//!
//! These compute byte ranges over the original source; callers convert them to
//! LSP `TextEdit`s via the document rope and wrap them in a `LintFix` /
//! `WorkspaceEdit`. Keeping the structural logic here (rather than in any one
//! lint) lets every fix source share one correct implementation of fiddly
//! operations like "remove a comma-separated item without leaving a dangling
//! comma".

use std::ops::Range;

use tree_sitter_generate::nativedsl::ast::Span;

/// Byte range that deletes a comma-separated `item` cleanly.
///
/// Comment-ownership-aware (the tsg language has only `//` line comments). When
/// the item is alone on its line (a multi-line collection), its whole line goes
/// (the item, its comma, and any trailing same-line comment), while the next
/// item's own-line leading comments stay put. When the item is inline
/// (single-line collection), it's the item plus its trailing comma and one
/// space, or a preceding comma when it's last. A sole item is deleted alone,
/// leaving an empty `[]` / `{}`.
///
/// Works for list elements and object fields alike - pass the item's span. To
/// drop an entire object field, widen the span first with [`object_field_span`].
#[must_use]
pub fn comma_item_deletion(source: &str, item: Span) -> Range<usize> {
    let b = source.as_bytes();
    let start = item.start as usize;
    let end = item.end as usize;

    // Multi-line: the item is alone on its line (only indentation between it and
    // the preceding newline). Delete the whole line - through the comma and a
    // trailing same-line comment - so nothing dangles and the next item's
    // leading comments are left intact.
    let indent = back_while(b, start, |c| c == b' ' || c == b'\t');
    if indent > 0 && b[indent - 1] == b'\n' {
        return (indent - 1)..item_line_end(b, end);
    }

    // Inline (single-line): item + trailing comma + a space, or a preceding
    // comma when the item is last.
    let after = skip_spaces(b, end);
    if b.get(after) == Some(&b',') {
        return start..skip_spaces(b, after + 1);
    }
    let before = back_while(b, start, |c| c.is_ascii_whitespace());
    if before > 0 && b[before - 1] == b',' {
        return (before - 1)..end;
    }

    // Sole item: remove just it.
    start..end
}

/// Walk `p` backward while `pred` holds for the preceding byte.
fn back_while(b: &[u8], mut p: usize, pred: impl Fn(u8) -> bool) -> usize {
    while p > 0 && pred(b[p - 1]) {
        p -= 1;
    }
    p
}

/// Skip spaces and tabs (not newlines) forward.
fn skip_spaces(b: &[u8], mut p: usize) -> usize {
    while p < b.len() && matches!(b[p], b' ' | b'\t') {
        p += 1;
    }
    p
}

/// End of the item's line: from just after the item, past its trailing comma
/// and any trailing same-line `//` comment, stopping before the line's newline.
/// The comma is optional (a last item in a no-trailing-comma list still deletes
/// cleanly as a whole line).
fn item_line_end(b: &[u8], end: usize) -> usize {
    let mut p = skip_spaces(b, end);
    if b.get(p) == Some(&b',') {
        p += 1;
    }
    p = skip_spaces(b, p);
    if b[p..].starts_with(b"//") {
        while p < b.len() && b[p] != b'\n' {
            p += 1;
        }
    }
    p
}

/// Widen an object field's *value* span back over its `key:`.
///
/// The result covers the whole `key: value` field (e.g. given the span of
/// `[...]` in `conflicts: [...]`, returns the span of `conflicts: [...]`).
/// Returns the value span unchanged if the expected `:` isn't found.
#[must_use]
pub fn object_field_span(source: &str, value: Span) -> Span {
    let bytes = source.as_bytes();
    let mut i = value.start as usize;
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    if i == 0 || bytes[i - 1] != b':' {
        return value;
    }
    i -= 1; // the colon
    while i > 0 && bytes[i - 1].is_ascii_whitespace() {
        i -= 1;
    }
    while i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
        i -= 1;
    }
    Span::new(i as u32, value.end)
}

#[cfg(test)]
mod tests {
    use super::{comma_item_deletion, object_field_span};
    use tree_sitter_generate::nativedsl::ast::Span;

    fn span_of(s: &str, needle: &str) -> Span {
        let start = s.find(needle).unwrap();
        Span::new(start as u32, (start + needle.len()) as u32)
    }

    fn delete(s: &str, item: &str) -> String {
        let mut out = s.to_string();
        out.replace_range(comma_item_deletion(s, span_of(s, item)), "");
        out
    }

    #[test]
    fn deletes_first_item_with_trailing_comma() {
        assert_eq!(delete("[[a, b], [c, d]]", "[a, b]"), "[[c, d]]");
    }

    #[test]
    fn deletes_last_item_with_leading_comma() {
        assert_eq!(delete("[[c, d], [a, b]]", "[a, b]"), "[[c, d]]");
    }

    #[test]
    fn deletes_sole_item_leaving_empty() {
        assert_eq!(delete("[[a, b]]", "[a, b]"), "[]");
    }

    #[test]
    fn deletes_multiline_item_with_comma_and_indent() {
        assert_eq!(
            delete("[\n    [a, b],\n    [c, d],\n]", "[a, b]"),
            "[\n    [c, d],\n]"
        );
    }

    #[test]
    fn deletes_last_item_trailing_comma_multiline() {
        // Last item before the closer (trailing-comma style): take the leading
        // newline + indent so nothing dangles before `]`.
        assert_eq!(
            delete("[\n    [c, d],\n    [a, b],\n]", "[a, b]"),
            "[\n    [c, d],\n]"
        );
    }

    #[test]
    fn deletes_item_with_trailing_line_comment() {
        // A `//` comment on the item's line is the item's; it goes too.
        assert_eq!(
            delete("[\n    [a, b], // foo\n    [c, d],\n]", "[a, b]"),
            "[\n    [c, d],\n]"
        );
    }

    #[test]
    fn deletes_last_item_with_trailing_line_comment() {
        assert_eq!(
            delete("[\n    [c, d],\n    [a, b], // foo\n]", "[a, b]"),
            "[\n    [c, d],\n]"
        );
    }

    #[test]
    fn preserves_next_items_leading_comment() {
        // A comment on its own line belongs to the item below it, so removing
        // the item above must leave it.
        assert_eq!(
            delete("[\n    [a, b],\n    // keep\n    [c, d],\n]", "[a, b]"),
            "[\n    // keep\n    [c, d],\n]"
        );
    }

    #[test]
    fn object_field_span_covers_key_and_value() {
        // `conflicts: [x]` inside a larger object: widening the `[x]` value
        // span reaches back over `conflicts: `.
        let s = "{\n    language: \"t\",\n    conflicts: [x],\n}";
        let value = span_of(s, "[x]");
        let field = object_field_span(s, value);
        assert_eq!(
            &s[field.start as usize..field.end as usize],
            "conflicts: [x]"
        );
    }

    #[test]
    fn delete_whole_object_field() {
        // Compose the two: widen to the field, then delete it with its comma.
        let s = "{\n    language: \"t\",\n    conflicts: [x],\n}";
        let field = object_field_span(s, span_of(s, "[x]"));
        let mut out = s.to_string();
        out.replace_range(comma_item_deletion(s, field), "");
        assert_eq!(out, "{\n    language: \"t\",\n}");
    }

    #[test]
    fn object_field_span_without_colon_returns_value() {
        // Defensive: a bare value (no `key:`) is returned unchanged.
        let s = "[x]";
        let value = span_of(s, "[x]");
        assert_eq!(object_field_span(s, value), value);
    }
}
