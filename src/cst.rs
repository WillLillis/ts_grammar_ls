//! Concrete-syntax-tree renderer for `tree_sitter::Tree`.
//!
//! Vendored from `tree_sitter_cli::parse` (the `--output-cst` mode of
//! the `tree-sitter parse` command) with two substitutions:
//!
//!   - The `anstyle::Color` type is replaced by [`ColorKind`]
//!   - Instead of writing ANSI escapes, the renderer records each
//!     colored region as a `(byte_start, byte_end, ColorKind)` span
//!     alongside the plain text. The LSP semantic-tokens handler
//!     converts these spans to LSP token deltas to provide highlighting.

use std::fmt::{self, Write as _};

use tree_sitter::{Range, Tree, TreeCursor};

/// Tag for which theme slot a piece of rendered text belongs to.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ColorKind {
    NodeKind,
    NodeText,
    Field,
    RowColor,
    RowColorNamed,
    Extra,
    Error,
    Missing,
    LineFeed,
    Backtick,
    Literal,
}

/// Theme: which color (if any) is associated with each renderable
/// category.
#[derive(Copy, Clone, Debug)]
pub struct ParseTheme {
    pub node_kind: Option<ColorKind>,
    pub node_text: Option<ColorKind>,
    pub field: Option<ColorKind>,
    pub row_color: Option<ColorKind>,
    pub row_color_named: Option<ColorKind>,
    pub extra: Option<ColorKind>,
    pub error: Option<ColorKind>,
    pub missing: Option<ColorKind>,
    pub line_feed: Option<ColorKind>,
    pub backtick: Option<ColorKind>,
    pub literal: Option<ColorKind>,
}

impl ParseTheme {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            node_kind: None,
            node_text: None,
            field: None,
            row_color: None,
            row_color_named: None,
            extra: None,
            error: None,
            missing: None,
            line_feed: None,
            backtick: None,
            literal: None,
        }
    }
}

impl Default for ParseTheme {
    fn default() -> Self {
        Self {
            node_kind: Some(ColorKind::NodeKind),
            node_text: Some(ColorKind::NodeText),
            field: Some(ColorKind::Field),
            row_color: Some(ColorKind::RowColor),
            row_color_named: Some(ColorKind::RowColorNamed),
            extra: Some(ColorKind::Extra),
            error: Some(ColorKind::Error),
            missing: Some(ColorKind::Missing),
            line_feed: Some(ColorKind::LineFeed),
            backtick: Some(ColorKind::Backtick),
            literal: Some(ColorKind::Literal),
        }
    }
}

/// One colored region of the rendered tree text. Half-open byte
/// range `[start, end)` into [`CstRender::text`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CstSpan {
    pub start: usize,
    pub end: usize,
    pub kind: ColorKind,
}

/// Output of [`render`]: the plain rendered tree text plus a list of
/// colored byte ranges into it (in source order).
#[derive(Debug, Default)]
pub struct CstRender {
    pub text: String,
    pub spans: Vec<CstSpan>,
}

/// Render `tree` as a CST.
#[must_use]
pub fn render(source: &[u8], tree: &Tree) -> CstRender {
    render_with_theme(source, tree, &ParseTheme::default())
}

#[must_use]
pub fn render_with_theme(source: &[u8], tree: &Tree, theme: &ParseTheme) -> CstRender {
    let mut r = Renderer::default();
    let lossy = String::from_utf8_lossy(source);
    let total_width = lossy
        .lines()
        .enumerate()
        .map(|(row, col)| {
            row.checked_ilog10().unwrap_or(0) as usize
                + col.len().checked_ilog10().unwrap_or(0) as usize
                + 1
        })
        .max()
        .unwrap_or(1);
    let mut cursor = tree.walk();
    let mut indent_level = 1usize;
    let mut did_visit_children = false;
    let mut in_error = false;
    loop {
        if did_visit_children {
            if cursor.goto_next_sibling() {
                did_visit_children = false;
            } else if cursor.goto_parent() {
                did_visit_children = true;
                indent_level -= 1;
                if !cursor.node().has_error() {
                    in_error = false;
                }
            } else {
                break;
            }
        } else {
            render_node(
                theme,
                &cursor,
                source,
                &mut r,
                total_width,
                indent_level,
                in_error,
            );
            if cursor.goto_first_child() {
                did_visit_children = false;
                indent_level += 1;
                if cursor.node().has_error() {
                    in_error = true;
                }
            } else {
                did_visit_children = true;
            }
        }
    }

    CstRender {
        text: r.text,
        spans: r.spans,
    }
}

/// Mutable rendering state.
#[derive(Default)]
struct Renderer {
    text: String,
    spans: Vec<CstSpan>,
}

impl Renderer {
    /// Write plain (uncolored) text. No span is emitted.
    fn push_str(&mut self, s: &str) {
        self.text.push_str(s);
    }

    fn push_char(&mut self, c: char) {
        self.text.push(c);
    }

    /// Write `text` and, if `color` is `Some`, record a span covering
    /// the bytes that were just appended.
    fn push_styled_str(&mut self, color: Option<ColorKind>, text: &str) {
        let start = self.text.len();
        self.text.push_str(text);
        if let Some(kind) = color {
            self.spans.push(CstSpan {
                start,
                end: self.text.len(),
                kind,
            });
        }
    }

    /// Write a single colored character.
    fn push_styled_char(&mut self, color: Option<ColorKind>, c: char) {
        let start = self.text.len();
        self.text.push(c);
        if let Some(kind) = color {
            self.spans.push(CstSpan {
                start,
                end: self.text.len(),
                kind,
            });
        }
    }

    /// Format the given [`fmt::Display`]/[`fmt::Arguments`] argument
    /// into the buffer and tag the resulting bytes with `color`.
    fn push_styled_fmt(&mut self, color: Option<ColorKind>, args: fmt::Arguments<'_>) {
        let start = self.text.len();
        let _ = write!(self.text, "{args}");
        if let Some(kind) = color {
            self.spans.push(CstSpan {
                start,
                end: self.text.len(),
                kind,
            });
        }
    }

    /// Write text with each character escape-transformed (matching
    /// upstream's `CstNodeText`) and tag the bytes with `color`.
    /// Escapes are applied first; the entire resulting region is one
    /// span (the transform doesn't switch color mid-character).
    fn push_styled_escaped(&mut self, color: Option<ColorKind>, source: &str) {
        let start = self.text.len();
        for c in source.chars() {
            match escape_invisible(c).or_else(|| escape_delimiter(c)) {
                Some(esc) => self.text.push_str(esc),
                None => self.text.push(c),
            }
        }
        if let Some(kind) = color {
            self.spans.push(CstSpan {
                start,
                end: self.text.len(),
                kind,
            });
        }
    }
}

fn render_node(
    theme: &ParseTheme,
    cursor: &TreeCursor,
    source_code: &[u8],
    r: &mut Renderer,
    total_width: usize,
    indent_level: usize,
    in_error: bool,
) {
    let node = cursor.node();
    let is_named = node.is_named();
    push_node_range(
        r,
        theme,
        cursor.field_name().is_some(),
        is_named,
        false,
        total_width,
        node.range(),
    );
    r.push_str(&"  ".repeat(indent_level));
    if in_error && !node.has_error() {
        r.push_str(" ");
    }
    if is_named {
        if let Some(field_name) = cursor.field_name() {
            r.push_styled_fmt(theme.field, format_args!("{field_name}: "));
        }
        if node.has_error() || node.is_error() {
            r.push_styled_str(theme.error, "•");
        }
        let kind_color = if node.is_error() {
            theme.error
        } else if node.is_extra() || node.parent().is_some_and(|p| p.is_extra() && !p.is_error()) {
            theme.extra
        } else {
            theme.node_kind
        };
        r.push_styled_str(kind_color, node.kind());
        if node.child_count() == 0 {
            let text = String::from_utf8_lossy(&source_code[node.start_byte()..node.end_byte()]);
            write_node_text(
                theme,
                r,
                cursor,
                is_named,
                &text,
                theme.node_text,
                (total_width, indent_level),
            );
        }
    } else if node.is_missing() {
        r.push_styled_str(theme.missing, "MISSING");
        r.push_str(": \"");
        r.push_styled_str(theme.missing, node.kind());
        r.push_str("\"");
    } else {
        write_node_text(
            theme,
            r,
            cursor,
            is_named,
            node.kind(),
            theme.literal,
            (total_width, indent_level),
        );
    }
    r.push_char('\n');
}

fn write_node_text(
    theme: &ParseTheme,
    r: &mut Renderer,
    cursor: &TreeCursor,
    is_named: bool,
    source: &str,
    color: Option<ColorKind>,
    text_info: (usize, usize),
) {
    let (total_width, indent_level) = text_info;
    let (quote, quote_color) = if is_named {
        ('`', theme.backtick)
    } else {
        ('"', color)
    };

    if is_named {
        let multiline = source.contains('\n');
        for (i, line) in source.split_inclusive('\n').enumerate() {
            if line.is_empty() {
                break;
            }
            let mut node_range = cursor.node().range();
            // For each line of text, adjust the row by shifting it down
            // `i` rows; adjust the column to the length of *this* line.
            node_range.start_point.row += i;
            node_range.end_point.row = node_range.start_point.row;
            node_range.end_point.column = line.len()
                + if i == 0 {
                    node_range.start_point.column
                } else {
                    0
                };
            if multiline {
                r.push_char('\n');
                push_node_range(
                    r,
                    theme,
                    cursor.field_name().is_some(),
                    is_named,
                    true,
                    total_width,
                    node_range,
                );
                for _ in 0..=indent_level {
                    r.push_str("  ");
                }
            } else {
                r.push_char(' ');
            }
            r.push_styled_char(quote_color, quote);
            push_line_feed(r, theme, color, line);
            r.push_styled_char(quote_color, quote);
        }
    } else {
        r.push_styled_char(quote_color, quote);
        r.push_styled_escaped(color, source);
        r.push_styled_char(quote_color, quote);
    }
}

/// Push the row-range prefix
fn push_node_range(
    r: &mut Renderer,
    theme: &ParseTheme,
    has_field_name: bool,
    is_named: bool,
    is_multiline: bool,
    total_width: usize,
    range: Range,
) {
    let start = range.start_point;
    let end = range.end_point;
    let range_color = if is_named && !is_multiline && !has_field_name {
        theme.row_color_named
    } else {
        theme.row_color
    };
    let remaining_width = |row: usize, col: usize| {
        total_width
            .saturating_sub(row.checked_ilog10().unwrap_or(0) as usize)
            .saturating_sub(col.checked_ilog10().unwrap_or(0) as usize)
            .max(1)
    };
    let remaining_width_start = remaining_width(start.row, start.column);
    let remaining_width_end = remaining_width(end.row, end.column);
    r.push_styled_fmt(
        range_color,
        format_args!(
            "{}:{}{:remaining_width_start$}- {}:{}{:remaining_width_end$}",
            start.row, start.column, ' ', end.row, end.column, ' ',
        ),
    );
}

/// Push a line's text with embedded linefeed markers. Replaces
/// upstream's `CstLineFeed` Display impl: the regular text gets
/// `text_color`, the linefeed markers get `theme.line_feed`, and the
/// two alternate.
fn push_line_feed(
    r: &mut Renderer,
    theme: &ParseTheme,
    text_color: Option<ColorKind>,
    source: &str,
) {
    #[cfg(windows)]
    let lf = "\r\n";
    #[cfg(not(windows))]
    let lf = "\n";
    // Display form of the linefeed (e.g. "\\n"). Built once because
    // we emit it between every pair of source segments.
    let mut lf_escaped = String::new();
    for c in lf.chars() {
        match escape_invisible(c).or_else(|| escape_delimiter(c)) {
            Some(esc) => lf_escaped.push_str(esc),
            None => lf_escaped.push(c),
        }
    }
    let mut parts = source.split(lf);
    if let Some(first) = parts.next() {
        r.push_styled_escaped(text_color, first);
    }
    for part in parts {
        r.push_styled_str(theme.line_feed, &lf_escaped);
        r.push_styled_escaped(text_color, part);
    }
}

const fn escape_invisible(c: char) -> Option<&'static str> {
    Some(match c {
        '\n' => "\\n",
        '\r' => "\\r",
        '\t' => "\\t",
        '\0' => "\\0",
        '\\' => "\\\\",
        '\x0b' => "\\v",
        '\x0c' => "\\f",
        _ => return None,
    })
}

const fn escape_delimiter(c: char) -> Option<&'static str> {
    Some(match c {
        '`' => "\\`",
        '"' => "\\\"",
        _ => return None,
    })
}
