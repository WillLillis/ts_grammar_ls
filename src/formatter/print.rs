//! Doc IR -> String renderer with width-aware group breaks.
//!
//! Two-mode emission: each `Group` is tried flat first, falling back to
//! broken if the flat layout would exceed the width budget. Probe is a
//! single forward walk that exits as soon as the budget is exceeded.

use super::doc::{DocArena, DocId, DocNode};

/// Project-wide default indent (matches `FormattingConfig::default()`).
pub const DEFAULT_INDENT_WIDTH: usize = 4;
/// Project-wide default line budget (matches `FormattingConfig::default()`).
pub const DEFAULT_MAX_WIDTH: usize = 100;

/// Render a Doc using the default indent (4) and width (100). Convenient
/// for tests and one-off uses where the caller doesn't need custom layout.
#[must_use]
pub fn render(arena: &DocArena, root: DocId) -> String {
    render_with_opts(arena, root, DEFAULT_INDENT_WIDTH, DEFAULT_MAX_WIDTH)
}

/// Render a Doc with explicit indent and width budget. Production callers
/// (the formatter entry) pass these from `FormattingConfig`.
#[must_use]
pub fn render_with_opts(
    arena: &DocArena,
    root: DocId,
    indent_width: usize,
    max_width: usize,
) -> String {
    let mut out = String::new();
    let mut state = RenderState {
        arena,
        out: &mut out,
        col: 0,
        indent_level: 0,
        indent_width,
        max_width,
    };
    state.emit(root, false);
    // Post-process: ensure exactly one trailing newline.
    while out.ends_with('\n') {
        out.pop();
    }
    out.push('\n');
    out
}

struct RenderState<'a> {
    arena: &'a DocArena,
    out: &'a mut String,
    col: usize,
    indent_level: usize,
    indent_width: usize,
    max_width: usize,
}

impl RenderState<'_> {
    fn emit(&mut self, id: DocId, broken: bool) {
        match self.arena.get(id) {
            DocNode::Text(s) => {
                self.out.push_str(s);
                self.col += s.chars().count();
            }
            DocNode::Raw(s) => self.emit_raw(s),
            DocNode::Line => self.newline(),
            DocNode::SoftLine => {
                if broken {
                    self.newline();
                } else {
                    self.out.push(' ');
                    self.col += 1;
                }
            }
            DocNode::SoftBreak => {
                if broken {
                    self.newline();
                }
            }
            DocNode::Group(child) => {
                let child = *child;
                // Try flat; if it doesn't fit at the current column, break.
                let flat_fits = self.fits_flat(child, self.max_width.saturating_sub(self.col));
                self.emit(child, !flat_fits);
            }
            DocNode::Fill(child) => self.emit_packed(*child),
            DocNode::Indent(child) => {
                let child = *child;
                self.indent_level += 1;
                self.emit(child, broken);
                self.indent_level -= 1;
            }
            DocNode::Concat { start, len } => {
                let (start, len) = (*start, *len);
                for &child_id in self.arena.concat_children(start, len) {
                    self.emit(child_id, broken);
                }
            }
            DocNode::IfBroken { broken: b, flat: f } => {
                let pick = if broken { *b } else { *f };
                self.emit(pick, broken);
            }
        }
    }

    /// Emit verbatim text that may contain newlines. After a newline, the
    /// column resets and *no* indent is auto-inserted - the caller is
    /// responsible for indenting the raw content if needed.
    fn emit_raw(&mut self, s: &str) {
        self.out.push_str(s);
        if let Some(last_nl) = s.rfind('\n') {
            // Column is the length of the tail after the last newline.
            self.col = s[last_nl + 1..].chars().count();
        } else {
            self.col += s.chars().count();
        }
    }

    fn newline(&mut self) {
        // Trim trailing whitespace from the current line before breaking.
        while self.out.ends_with(' ') {
            self.out.pop();
        }
        self.out.push('\n');
        let indent = self.indent_level * self.indent_width;
        for _ in 0..indent {
            self.out.push(' ');
        }
        self.col = indent;
    }

    /// Probe whether `id` would fit in `budget` columns if every internal
    /// SoftLine/SoftBreak ran flat. Returns false the moment we'd exceed.
    /// Nested `Group`s are also probed flat (the outer flat decision is
    /// only meaningful if everything stays on one line).
    fn fits_flat(&self, id: DocId, budget: usize) -> bool {
        let mut probe = FitsProbe {
            arena: self.arena,
            remaining: budget,
        };
        probe.walk(id)
    }

    /// Pack-mode emission for `Fill`. Walks the child's top-level children
    /// (flattening one level of `Concat`) and makes a per-`SoftLine`
    /// decision: if the next chunk (up to the next `SoftLine`) fits in the
    /// remaining line budget, emit a space; otherwise emit a newline.
    fn emit_packed(&mut self, id: DocId) {
        let children: Vec<DocId> = match self.arena.get(id) {
            DocNode::Concat { start, len } => {
                self.arena.concat_children(*start, *len).to_vec()
            }
            _ => vec![id],
        };
        let mut i = 0;
        while i < children.len() {
            let c = children[i];
            if matches!(self.arena.get(c), DocNode::SoftLine) {
                let mut j = i + 1;
                while j < children.len()
                    && !matches!(self.arena.get(children[j]), DocNode::SoftLine)
                {
                    j += 1;
                }
                let budget = self.max_width.saturating_sub(self.col + 1);
                let mut probe = FitsProbe {
                    arena: self.arena,
                    remaining: budget,
                };
                let segment_fits = children[i + 1..j].iter().all(|&c| probe.walk(c));
                if segment_fits {
                    self.out.push(' ');
                    self.col += 1;
                } else {
                    self.newline();
                }
                i += 1;
            } else {
                // Inside a packed Fill, `broken` is true: IfBroken picks the
                // broken side, SoftLines we encounter as part of nested
                // structures still respect that.
                self.emit(c, true);
                i += 1;
            }
        }
    }
}

struct FitsProbe<'a> {
    arena: &'a DocArena,
    remaining: usize,
}

impl FitsProbe<'_> {
    /// Returns `true` iff the doc fits flat into `remaining`.
    fn walk(&mut self, id: DocId) -> bool {
        match self.arena.get(id) {
            DocNode::Text(s) => self.consume(s.chars().count()),
            DocNode::Raw(s) => {
                // A raw region with an internal newline can't be flat - the
                // surrounding group must break. Otherwise count its chars.
                if s.contains('\n') {
                    return false;
                }
                self.consume(s.chars().count())
            }
            // Hard line forces flat layout to fail: a flat group can't
            // contain a hard newline.
            DocNode::Line => false,
            // In flat mode, SoftLine -> " " (1 char), SoftBreak -> "".
            DocNode::SoftLine => self.consume(1),
            DocNode::SoftBreak => true,
            DocNode::Group(child) | DocNode::Indent(child) => self.walk(*child),
            DocNode::Concat { start, len } => {
                let (start, len) = (*start, *len);
                for &child_id in self.arena.concat_children(start, len) {
                    if !self.walk(child_id) {
                        return false;
                    }
                }
                true
            }
            // While probing flat, IfBroken takes the flat side.
            DocNode::IfBroken { flat, .. } => self.walk(*flat),
            // For a flat probe, a Fill's contents must fit flat too.
            DocNode::Fill(child) => self.walk(*child),
        }
    }

    fn consume(&mut self, n: usize) -> bool {
        if n > self.remaining {
            return false;
        }
        self.remaining -= n;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_only() {
        let mut a = DocArena::new();
        let id = a.text("hello");
        assert_eq!(render(&a, id), "hello\n");
    }

    #[test]
    fn group_flat() {
        let mut a = DocArena::new();
        let lparen = a.text("(");
        let body = a.text("x");
        let rparen = a.text(")");
        let parts = a.concat(&[lparen, body, rparen]);
        let g = a.group(parts);
        assert_eq!(render(&a, g), "(x)\n");
    }

    #[test]
    fn group_with_trailing_comma_only_when_broken() {
        let mut a = DocArena::new();
        // `seq(a, b)` modeled correctly: `,` between args is flat-space when
        // not broken, `,\n` when broken; trailing `,` appears only when
        // broken via IfBroken.
        let seq_open = a.text("seq(");
        let sb_open = a.softbreak();
        let a_tok = a.text("a");
        let comma_sep = a.text(",");
        let sl = a.softline();
        let b_tok = a.text("b");
        let nil = a.nil();
        let trailing_comma_text = a.text(",");
        let trailing_comma = a.if_broken(trailing_comma_text, nil);
        // Indent wraps the opening softbreak too: the first content line
        // after `seq(` needs the indent applied. The closing softbreak is
        // *outside* the indent so `)` returns to the outer column.
        let indented_block = a.concat(&[sb_open, a_tok, comma_sep, sl, b_tok, trailing_comma]);
        let indented = a.indent(indented_block);
        let sb_close = a.softbreak();
        let close = a.text(")");
        let full = a.concat(&[seq_open, indented, sb_close, close]);
        let g = a.group(full);
        // Flat: "seq(a, b)" - no trailing comma, single space between args.
        assert_eq!(render(&a, g), "seq(a, b)\n");
        // Broken: trailing comma added, each arg on its own line.
        let broken = render_with_opts(&a, g, 4, 5);
        assert_eq!(broken, "seq(\n    a,\n    b,\n)\n");
    }

    #[test]
    fn raw_with_newline_forces_break() {
        let mut a = DocArena::new();
        let pre = a.text("x");
        let r = a.raw("// comment\n");
        let post = a.text("y");
        let sb1 = a.softbreak();
        let sb2 = a.softbreak();
        let inner = a.concat(&[pre, sb1, r, sb2, post]);
        let g = a.group(inner);
        let out = render(&a, g);
        // Group must break because Raw contains a newline.
        assert!(out.contains('\n'), "got: {out:?}");
    }

    #[test]
    fn trailing_whitespace_trimmed_on_break() {
        let mut a = DocArena::new();
        let pre = a.text("foo ");
        let line = a.line();
        let post = a.text("bar");
        let full = a.concat(&[pre, line, post]);
        let out = render(&a, full);
        // Should be "foo\nbar\n" not "foo \nbar\n".
        assert_eq!(out, "foo\nbar\n");
    }
}
