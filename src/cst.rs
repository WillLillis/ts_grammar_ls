//! Concrete-syntax-tree renderer for `tree_sitter::Tree`.
//!
//! Vendored from `tree_sitter_cli::parse` (the `--output-cst` mode of
//! the `tree-sitter parse` command). The upstream renderer is bundled
//! with the CLI binary which carries a heavy dep set; copying the
//! relevant ~250 lines keeps our dep graph small. The original is at
//! `tree-sitter/crates/cli/src/parse.rs`; this version is faithful to
//! upstream's output structure with one substitution:
//!
//!   - The `anstyle::Color` type is replaced by a local [`ColorKind`]
//!     tag enum, and [`paint`] is a stub that writes plain text without
//!     emitting ANSI escapes. The call structure is preserved so that
//!     adding LSP semantic tokens later only requires changing the
//!     `paint` impl to record (range, kind) tuples instead of writing
//!     the text directly. Each `paint(theme.<field>, ...)` call site
//!     keeps its color category, which is what the semantic-tokens
//!     layer will eventually map to LSP token kinds.
//!
//! Public surface: [`render`] takes a [`Tree`] + source bytes, returns
//! the rendered CST as a `String`. Keep in sync with upstream's
//! renderer if its output format changes.

use std::fmt::{self, Write as _};

use tree_sitter::{Range, Tree, TreeCursor};

/// Tag for which theme slot a piece of rendered text belongs to.
/// Mirrors the field names of upstream's `ParseTheme` so semantic-token
/// mapping (future) can route each kind to a distinct LSP token type.
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
/// category. `Default` matches upstream's defaults symbolically;
/// [`Self::empty`] is no-color (every field `None`).
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

/// Wrapper that preserves upstream's `paint(color, text)` call shape.
/// Today it just writes the text. When we add LSP semantic tokens,
/// the `Display` impl will record the current output offset and the
/// `ColorKind` so the semantic-tokens handler can return matching
/// `(range, kind)` tuples; the call sites in [`render_node`] /
/// [`write_node_text`] won't change.
pub struct Paint<T>(#[allow(dead_code)] pub Option<ColorKind>, pub T);

#[must_use]
pub fn paint<T>(color: Option<ColorKind>, text: T) -> Paint<T> {
    Paint(color, text)
}

impl<T: fmt::Display> fmt::Display for Paint<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.1.fmt(f)
    }
}

/// Render `tree` as a CST in `tree-sitter parse --output-cst` format.
/// Source bytes are needed to recover the text of each leaf node.
#[must_use]
pub fn render(source: &[u8], tree: &Tree) -> String {
    render_with_theme(source, tree, &ParseTheme::empty())
}

#[must_use]
pub fn render_with_theme(source: &[u8], tree: &Tree, theme: &ParseTheme) -> String {
    let mut out = String::new();
    let lossy = String::from_utf8_lossy(source);
    let total_width = lossy
        .lines()
        .enumerate()
        .map(|(row, col)| {
            (row as f64).log10() as usize + (col.len() as f64).log10() as usize + 1
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
                &mut out,
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
    out
}

fn render_node(
    theme: &ParseTheme,
    cursor: &TreeCursor,
    source_code: &[u8],
    out: &mut String,
    total_width: usize,
    indent_level: usize,
    in_error: bool,
) {
    let node = cursor.node();
    let is_named = node.is_named();
    let _ = write!(
        out,
        "{}",
        CstNodeRange {
            theme,
            has_field_name: cursor.field_name().is_some(),
            is_named,
            is_multiline: false,
            total_width,
            range: node.range(),
        }
    );
    let _ = write!(
        out,
        "{}{}",
        "  ".repeat(indent_level),
        if in_error && !node.has_error() { " " } else { "" }
    );
    if is_named {
        if let Some(field_name) = cursor.field_name() {
            let _ = write!(out, "{}", paint(theme.field, format_args!("{field_name}: ")));
        }
        if node.has_error() || node.is_error() {
            let _ = write!(out, "{}", paint(theme.error, "•"));
        }
        let kind_color = if node.is_error() {
            theme.error
        } else if node.is_extra() || node.parent().is_some_and(|p| p.is_extra() && !p.is_error()) {
            theme.extra
        } else {
            theme.node_kind
        };
        let _ = write!(out, "{}", paint(kind_color, node.kind()));
        if node.child_count() == 0 {
            let text =
                String::from_utf8_lossy(&source_code[node.start_byte()..node.end_byte()]);
            write_node_text(
                theme,
                out,
                cursor,
                is_named,
                &text,
                theme.node_text,
                (total_width, indent_level),
            );
        }
    } else if node.is_missing() {
        let _ = write!(out, "{}: ", paint(theme.missing, "MISSING"));
        let _ = write!(out, "\"{}\"", paint(theme.missing, node.kind()));
    } else {
        write_node_text(
            theme,
            out,
            cursor,
            is_named,
            node.kind(),
            theme.literal,
            (total_width, indent_level),
        );
    }
    out.push('\n');
}

fn write_node_text(
    theme: &ParseTheme,
    out: &mut String,
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

    if !is_named {
        let _ = write!(
            out,
            "{}{}{}",
            paint(quote_color, quote),
            paint(color, CstNodeText(source)),
            paint(quote_color, quote),
        );
    } else {
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
                out.push('\n');
                let _ = write!(
                    out,
                    "{}",
                    CstNodeRange {
                        theme,
                        has_field_name: cursor.field_name().is_some(),
                        is_named,
                        is_multiline: true,
                        total_width,
                        range: node_range,
                    }
                );
                for _ in 0..=indent_level {
                    let _ = out.write_str("  ");
                }
            } else {
                let _ = out.write_char(' ');
            }
            let _ = write!(
                out,
                "{}{}{}",
                paint(quote_color, quote),
                paint(color, CstLineFeed { source: line, theme }),
                paint(quote_color, quote),
            );
        }
    }
}

struct CstNodeText<'a>(&'a str);

impl fmt::Display for CstNodeText<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for c in self.0.chars() {
            match escape_invisible(c).or_else(|| escape_delimiter(c)) {
                Some(esc) => f.write_str(esc)?,
                None => f.write_char(c)?,
            }
        }
        Ok(())
    }
}

struct CstLineFeed<'a> {
    source: &'a str,
    theme: &'a ParseTheme,
}

impl fmt::Display for CstLineFeed<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        #[cfg(windows)]
        let lf = "\r\n";
        #[cfg(not(windows))]
        let lf = "\n";
        let painted = paint(self.theme.line_feed, CstNodeText(lf));
        let mut parts = self.source.split(lf);
        if let Some(first) = parts.next() {
            write!(f, "{}", CstNodeText(first))?;
        }
        for part in parts {
            write!(f, "{painted}{}", CstNodeText(part))?;
        }
        Ok(())
    }
}

struct CstNodeRange<'a> {
    theme: &'a ParseTheme,
    has_field_name: bool,
    is_named: bool,
    is_multiline: bool,
    total_width: usize,
    range: Range,
}

impl fmt::Display for CstNodeRange<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let start = self.range.start_point;
        let end = self.range.end_point;
        let range_color = if self.is_named && !self.is_multiline && !self.has_field_name {
            self.theme.row_color_named
        } else {
            self.theme.row_color
        };
        let remaining_width = |row: usize, col: usize| {
            (self
                .total_width
                .saturating_sub((row as f64).log10() as usize)
                .saturating_sub((col as f64).log10() as usize))
            .max(1)
        };
        let remaining_width_start = remaining_width(start.row, start.column);
        let remaining_width_end = remaining_width(end.row, end.column);
        write!(
            f,
            "{}",
            paint(
                range_color,
                format_args!(
                    "{}:{}{:remaining_width_start$}- {}:{}{:remaining_width_end$}",
                    start.row, start.column, ' ', end.row, end.column, ' ',
                ),
            )
        )
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
