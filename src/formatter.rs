use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

use tree_sitter_generate::nativedsl::ast::{self, Ast, Node, NodeId, Span};
use tree_sitter_generate::nativedsl::lexer::{self, Token, TokenKind};

use crate::config::FormattingConfig;

/// Format a `.tsg` source file. Returns `None` if the source can't be parsed.
#[must_use]
pub fn format(source: &str, path: &Path, config: &FormattingConfig) -> Option<String> {
    let tokens = lexer::Lexer::new(source).tokenize().ok()?;
    let ast = tree_sitter_generate::nativedsl::parser::Parser::new(&tokens, source, path)
        .parse()
        .ok()?;

    let line_starts = compute_line_starts(source);

    let mut f = Formatter {
        source,
        ast: &ast,
        tokens: &tokens,
        config,
        comments: CommentMap::build(source, &tokens, &line_starts),
        line_starts,
        out: String::with_capacity(source.len()),
        indent: 0,
        dry_run: false,
    };

    f.format_file();
    Some(f.out)
}

/// Compute the byte offset of each line's start. `line_starts[0]` is always 0;
/// `line_starts[L]` is the offset of the byte after the L-th newline.
fn compute_line_starts(source: &str) -> Vec<u32> {
    let mut starts = vec![0u32];
    starts.extend(memchr::memchr_iter(b'\n', source.as_bytes()).map(|i| (i + 1) as u32));
    starts
}

/// Find the 0-based line number containing the given byte offset.
fn offset_to_line(line_starts: &[u32], offset: u32) -> usize {
    // partition_point returns the first index where line_starts[i] > offset.
    // The line containing offset is one less.
    line_starts
        .partition_point(|&s| s <= offset)
        .saturating_sub(1)
}

// ---------------------------------------------------------------------------
// Comment map
// ---------------------------------------------------------------------------

struct LeadingComment {
    text: String,
    blank_before: bool,
}

struct TrailingComment {
    text: String,
    line: usize,
    offset: u32,
}

/// Pre-scanned comment associations.
struct CommentMap {
    /// Comments to emit before the token at a given source offset.
    leading: BTreeMap<u32, Vec<LeadingComment>>,
    trailing: Vec<TrailingComment>,
    /// Source byte ranges where formatting is disabled (`// tsg-format: off` .. `// tsg-format: on`).
    off_regions: Vec<std::ops::Range<u32>>,
}

impl CommentMap {
    fn build(source: &str, tokens: &[Token], line_starts: &[u32]) -> Self {
        let mut leading: BTreeMap<u32, Vec<LeadingComment>> = BTreeMap::new();
        let mut trailing: Vec<TrailingComment> = Vec::new();
        let mut off_regions = Vec::new();
        let mut off_start: Option<u32> = None;

        for (i, token) in tokens.iter().enumerate() {
            if token.kind != TokenKind::Comment {
                continue;
            }

            let comment_text = source[token.span.start as usize..token.span.end as usize].trim();

            // Detect format toggle directives.
            if comment_text == "// tsg-format: off" {
                if off_start.is_none() {
                    off_start = Some(token.span.start);
                }
                continue;
            }
            if comment_text == "// tsg-format: on" {
                if let Some(start) = off_start.take() {
                    // Include the trailing newline after the "on" comment.
                    let end = source[token.span.end as usize..]
                        .find('\n')
                        .map_or(source.len(), |pos| token.span.end as usize + pos + 1);
                    off_regions.push(start..end as u32);
                }
                continue;
            }

            // Skip comments inside an off region - they'll be emitted verbatim.
            if off_start.is_some() {
                continue;
            }

            let comment_text = comment_text.to_owned();

            // Find the previous non-comment token.
            let prev = tokens[..i].iter().rfind(|t| t.kind != TokenKind::Comment);

            // Check if this comment is on the same line as the previous token
            // (trailing comment) by looking for a newline in the gap.
            let is_trailing = prev.is_some_and(|p| {
                !source[p.span.end as usize..token.span.start as usize].contains('\n')
            });

            if is_trailing {
                let line = offset_to_line(line_starts, token.span.start);
                trailing.push(TrailingComment {
                    text: comment_text,
                    line,
                    offset: token.span.start,
                });
            } else {
                // Use the immediately preceding token (including comments)
                // for blank line detection, so stacked comments don't all
                // get blank_before from a distant non-comment token.
                let prev_any = if i > 0 { Some(&tokens[i - 1]) } else { None };
                let gap_before = prev_any.map_or(&source[..token.span.start as usize], |p| {
                    &source[p.span.end as usize..token.span.start as usize]
                });
                let blank_before = gap_before.chars().filter(|&c| c == '\n').count() >= 2;

                let entry = LeadingComment {
                    text: comment_text,
                    blank_before,
                };

                let next = tokens[i + 1..]
                    .iter()
                    .find(|t| t.kind != TokenKind::Comment);
                if let Some(next) = next {
                    leading.entry(next.span.start).or_default().push(entry);
                } else {
                    leading.entry(token.span.start).or_default().push(entry);
                }
            }
        }

        // Unclosed off region extends to EOF.
        if let Some(start) = off_start {
            off_regions.push(start..source.len() as u32);
        }

        Self {
            leading,
            trailing,
            off_regions,
        }
    }

    /// Get leading comments for a node at the given source offset.
    fn leading_at(&self, offset: u32) -> &[LeadingComment] {
        self.leading.get(&offset).map_or(&[], |v| v.as_slice())
    }

    /// Remove and return the trailing comment on the given source line.
    fn take_trailing_on_line(&mut self, line: usize) -> Option<String> {
        let idx = self.trailing.iter().position(|c| c.line == line)?;
        Some(self.trailing.swap_remove(idx).text)
    }

    /// Check if there are any comments in the given byte offset range.
    fn has_comments_in(&self, start: u32, end: u32) -> bool {
        self.leading.range(start..end).next().is_some()
            || self
                .trailing
                .iter()
                .any(|c| c.offset >= start && c.offset < end)
    }
}

// ---------------------------------------------------------------------------
// Formatter
// ---------------------------------------------------------------------------

struct Formatter<'a> {
    source: &'a str,
    ast: &'a Ast<'a>,
    tokens: &'a [Token],
    config: &'a FormattingConfig,
    comments: CommentMap,
    /// Byte offset of each line's start, for O(log L) offset->line lookups.
    line_starts: Vec<u32>,
    out: String,
    indent: usize,
    /// When true, comment emission is suppressed. Used during speculative
    /// formatting (`format_expr_to_string`) to measure width without side effects.
    dry_run: bool,
}

impl<'a> Formatter<'a> {
    fn write_indent(&mut self) {
        let width = self.indent * self.config.indent_width;
        let _ = write!(self.out, "{:width$}", "");
    }

    /// Emit leading comments for the node at the given source offset.
    /// Consumes the comments from the map so they can't be emitted twice.
    fn emit_leading_comments(&mut self, offset: u32) {
        if self.dry_run {
            return;
        }
        if let Some(comments) = self.comments.leading.remove(&offset) {
            for comment in &comments {
                if comment.blank_before && !self.out.is_empty() && !self.out.ends_with("\n\n") {
                    self.out.push('\n');
                }
                self.write_indent();
                self.out.push_str(&comment.text);
                self.out.push('\n');
            }
        }
    }

    /// Append a trailing comment on the same source line as the given offset.
    fn emit_trailing_comment(&mut self, offset: u32) {
        if self.dry_run {
            return;
        }
        let line = offset_to_line(&self.line_starts, offset);
        if let Some(comment) = self.comments.take_trailing_on_line(line) {
            self.out.push(' ');
            self.out.push_str(&comment);
        }
    }

    fn format_file(&mut self) {
        let mut prev_kind: Option<ItemKind> = None;
        let mut off_idx = 0;
        let mut i = 0;

        while i < self.ast.root_items.len() {
            let item_id = self.ast.root_items[i];
            let item_span = self.ast.span(item_id);

            // Emit any off regions that end before this item (empty regions
            // containing only the directive comments with no items between).
            while off_idx < self.comments.off_regions.len()
                && self.comments.off_regions[off_idx].end <= item_span.start
            {
                let (start, end) = self.off_region_bounds(off_idx);
                self.emit_off_region(start, end);
                off_idx += 1;
                prev_kind = Some(ItemKind::Other);
            }

            // Check if this item falls inside the current off region.
            if off_idx < self.comments.off_regions.len()
                && item_span.start >= self.comments.off_regions[off_idx].start
            {
                let (start, end) = self.off_region_bounds(off_idx);
                self.emit_off_region(start, end);
                // Skip all items within this off region.
                while i < self.ast.root_items.len()
                    && self.ast.span(self.ast.root_items[i]).start < end
                {
                    i += 1;
                }
                off_idx += 1;
                prev_kind = Some(ItemKind::Other);
                continue;
            }

            let kind = item_kind(self.ast, item_id);

            // Blank line between different kinds of items, or between rules.
            if let Some(pk) = prev_kind
                && (pk != kind || kind == ItemKind::Rule)
                && !self.out.ends_with("\n\n")
            {
                self.out.push('\n');
            }

            // Emit leading comments for this item.
            self.emit_leading_comments(self.ast.span(item_id).start);

            match self.ast.node(item_id) {
                ast::Node::Grammar => {
                    self.format_grammar_block(self.ast.context.grammar_config.as_ref().unwrap());
                }
                ast::Node::Fn(fn_idx) => self.format_fn(item_id, *fn_idx),
                ast::Node::Let { name, ty, value } => {
                    self.format_let(item_id, *name, *ty, *value);
                }
                ast::Node::Rule { name, body } => {
                    self.format_rule("rule", *name, *body);
                }
                ast::Node::OverrideRule { name, body } => {
                    self.format_rule("override rule", *name, *body);
                }
                ast::Node::Print(arg) => {
                    self.out.push_str("print(");
                    self.format_expr(*arg);
                    self.out.push(')');
                }
                _ => {
                    let span = self.ast.span(item_id);
                    self.out.push_str(self.text(span));
                }
            }

            self.emit_trailing_comment(self.ast.span(item_id).end);
            self.out.push('\n');

            prev_kind = Some(kind);
            i += 1;
        }

        // Emit any remaining off regions after all items.
        while off_idx < self.comments.off_regions.len() {
            let (start, end) = self.off_region_bounds(off_idx);
            self.emit_off_region(start, end);
            off_idx += 1;
        }

        // Emit any trailing comments at end of file (attached to EOF token).
        let Some(Token {
            kind: TokenKind::Eof,
            span: eof_span,
        }) = self.tokens.last()
        else {
            unreachable!();
        };
        self.emit_leading_comments(eof_span.start);
    }

    fn off_region_bounds(&self, idx: usize) -> (u32, u32) {
        let r = &self.comments.off_regions[idx];
        (r.start, r.end)
    }

    /// Emit a `// tsg-format: off` .. `// tsg-format: on` region verbatim.
    fn emit_off_region(&mut self, start: u32, end: u32) {
        if !self.out.is_empty() && !self.out.ends_with("\n\n") {
            self.out.push('\n');
        }
        let verbatim = self.source[start as usize..end as usize].trim_end();
        self.out.push_str(verbatim);
        self.out.push('\n');
    }

    /// Find the byte offset of an identifier token matching `name` within a source range.
    fn find_keyword_in_range(&self, name: &str, start: u32, end: u32) -> Option<u32> {
        self.tokens.iter().find_map(|t| {
            if t.kind == TokenKind::Ident
                && t.span.start >= start
                && t.span.end <= end
                && self.text(t.span) == name
            {
                Some(t.span.start)
            } else {
                None
            }
        })
    }

    fn format_grammar_block(&mut self, config: &ast::GrammarConfig) {
        self.out.push_str("grammar {\n");
        self.indent += 1;

        let grammar_span = self
            .ast
            .root_items
            .iter()
            .find(|&&id| matches!(self.ast.node(id), ast::Node::Grammar))
            .map(|&id| self.ast.span(id))
            .unwrap();

        // Emit comments for each field by looking up comments associated
        // with the field's keyword token, not by scanning byte ranges
        // (which would pick up comments inside other fields' values).
        macro_rules! emit_field {
            ($field:expr, $name:expr, $id:expr) => {
                if let Some(id) = $id {
                    if let Some(kw) =
                        self.find_keyword_in_range($name, grammar_span.start, grammar_span.end)
                    {
                        self.emit_leading_comments(kw);
                    }
                    self.write_indent();
                    self.out.push_str(concat!($name, ": "));
                    $field(self, id);
                    self.out.push_str(",\n");
                }
            };
        }

        if let Some(ref lang) = config.language {
            if let Some(kw) =
                self.find_keyword_in_range("language", grammar_span.start, grammar_span.end)
            {
                self.emit_leading_comments(kw);
            }
            self.write_indent();
            let _ = writeln!(self.out, "language: \"{lang}\",");
        }

        emit_field!(Self::format_expr, "word", config.word);
        emit_field!(Self::format_expr, "externals", config.externals);
        emit_field!(Self::format_expr, "extras", config.extras);
        emit_field!(Self::format_expr, "supertypes", config.supertypes);
        emit_field!(Self::format_expr, "inline", config.inline);

        emit_field!(Self::format_expr, "precedences", config.precedences);
        emit_field!(Self::format_expr, "conflicts", config.conflicts);
        emit_field!(Self::format_expr, "reserved", config.reserved);
        emit_field!(Self::format_expr, "inherits", config.inherits);

        self.indent -= 1;
        self.out.push('}');
    }

    fn format_rule(&mut self, keyword: &str, name: NodeId, body: NodeId) {
        let name_str = self.text(self.ast.span(name));
        let body_span = self.ast.span(body);
        let has_body_comments = self
            .comments
            .has_comments_in(body_span.start, body_span.end);

        if !has_body_comments {
            let body_str = self.format_expr_to_string(body);
            let one_liner = format!("{keyword} {name_str} {{ {body_str} }}");
            if self.fits_on_line(&one_liner) {
                self.out.push_str(&one_liner);
                return;
            }
        }

        let _ = writeln!(self.out, "{keyword} {name_str} {{");
        self.indent += 1;
        self.write_indent();
        self.format_expr(body);
        self.out.push('\n');
        self.indent -= 1;
        self.out.push('}');
    }

    fn format_fn(&mut self, _item_id: NodeId, fn_idx: ast::FnId) {
        let config = self.ast.get_fn(fn_idx);
        let fn_name = self.text(self.ast.span(config.name));

        let params: Vec<String> = config
            .params
            .iter()
            .map(|p| {
                let pname = self.text(self.ast.span(p.name));
                let pty = self.type_str(p.ty);
                format!("{pname}: {pty}")
            })
            .collect();
        let params_str = params.join(", ");
        let return_ty = self.type_str(config.return_ty).to_owned();

        let body_span = self.ast.span(config.body);
        let has_body_comments = self
            .comments
            .has_comments_in(body_span.start, body_span.end);

        if !has_body_comments {
            let body_str = self.format_expr_to_string(config.body);
            let one_liner = format!("fn {fn_name}({params_str}) -> {return_ty} {{ {body_str} }}");
            if self.fits_on_line(&one_liner) {
                self.out.push_str(&one_liner);
                return;
            }
        }

        let _ = writeln!(self.out, "fn {fn_name}({params_str}) -> {return_ty} {{");
        self.indent += 1;
        self.write_indent();
        self.format_expr(config.body);
        self.out.push('\n');
        self.indent -= 1;
        self.out.push('}');
    }

    fn format_let(&mut self, _item_id: NodeId, name: NodeId, ty: Option<NodeId>, value: NodeId) {
        let name_str = self.text(self.ast.span(name));
        let ty_str = ty.map(|t| format!(": {}", self.type_str(t)));
        let value_str = self.format_expr_to_string(value);

        let one_liner = format!(
            "let {name_str}{} = {value_str}",
            ty_str.as_deref().unwrap_or("")
        );

        if self.fits_on_line(&one_liner) {
            self.out.push_str(&one_liner);
        } else {
            write!(
                self.out,
                "let {name_str}{} =",
                ty_str.as_deref().unwrap_or("")
            )
            .unwrap();
            match self.ast.node(value) {
                ast::Node::Object(_) | ast::Node::List(_) => {
                    self.out.push(' ');
                    self.format_expr(value);
                }
                _ => {
                    self.out.push('\n');
                    self.indent += 1;
                    self.write_indent();
                    self.format_expr(value);
                    self.indent -= 1;
                }
            }
        }
    }

    #[expect(clippy::too_many_lines)]
    fn format_expr(&mut self, id: NodeId) {
        match self.ast.node(id) {
            Node::StringLit => {
                self.out.push('"');
                self.out.push_str(self.text(self.ast.span(id)));
                self.out.push('"');
            }
            Node::IntLit(n) => self.out.push_str(&n.to_string()),
            Node::Neg(inner) => {
                self.out.push('-');
                self.format_expr(*inner);
            }
            Node::Ident | Node::RuleRef | Node::VarRef | Node::RawStringLit { .. } => {
                self.out.push_str(self.text(self.ast.span(id)));
            }
            Node::Seq(range) => self.format_variadic("seq", *range),
            Node::Choice(range) => self.format_variadic("choice", *range),
            Node::List(range) => {
                self.format_bracketed('[', ']', self.ast.child_slice(*range));
            }
            Node::Tuple(range) => {
                self.format_bracketed('(', ')', self.ast.child_slice(*range));
            }
            Node::Object(range) => self.format_object(*range),
            Node::Repeat(inner) => self.format_unary("repeat", *inner),
            Node::Repeat1(inner) => self.format_unary("repeat1", *inner),
            Node::Optional(inner) => self.format_unary("optional", *inner),
            Node::Token(inner) => self.format_unary("token", *inner),
            Node::TokenImmediate(inner) => self.format_unary("token_immediate", *inner),
            Node::Blank => self.out.push_str("blank()"),
            Node::Field { name, content } => {
                self.out.push_str("field(");
                self.out.push_str(self.text(self.ast.span(*name)));
                self.out.push_str(", ");
                self.format_expr(*content);
                self.out.push(')');
            }
            Node::Alias { content, target } => {
                self.out.push_str("alias(");
                self.format_expr(*content);
                self.out.push_str(", ");
                self.format_expr(*target);
                self.out.push(')');
            }
            Node::Prec { value, content } => self.format_binary("prec", *value, *content),
            Node::PrecLeft { value, content } => {
                self.format_binary("prec_left", *value, *content);
            }
            Node::PrecRight { value, content } => {
                self.format_binary("prec_right", *value, *content);
            }
            Node::PrecDynamic { value, content } => {
                self.format_binary("prec_dynamic", *value, *content);
            }
            Node::DynRegex { pattern, flags } => {
                self.out.push_str("regexp(");
                self.format_expr(*pattern);
                if let Some(f) = flags {
                    self.out.push_str(", ");
                    self.format_expr(*f);
                }
                self.out.push(')');
            }
            Node::Concat(range) => self.format_variadic("concat", *range),
            Node::Append { left, right } => {
                self.format_binary("append", *left, *right);
            }
            Node::Reserved { context, content } => {
                self.format_binary("reserved", *context, *content);
            }
            Node::Inherit { path } => {
                self.out.push_str("inherit(");
                self.format_expr(*path);
                self.out.push(')');
            }
            Node::FieldAccess { obj, field } => {
                self.format_expr(*obj);
                self.out.push('.');
                self.out.push_str(self.text(self.ast.span(*field)));
            }
            Node::RuleInline { obj, rule } => {
                self.format_expr(*obj);
                self.out.push_str("::");
                self.out.push_str(self.text(self.ast.span(*rule)));
            }
            Node::Call { name, args } => {
                self.out.push_str(self.text(self.ast.span(*name)));
                let children = self.ast.child_slice(*args);
                self.format_call_args(children);
            }
            Node::For(for_id) => self.format_for(*for_id),
            Node::TypeRule => self.out.push_str("rule_t"),
            Node::TypeStr => self.out.push_str("str_t"),
            Node::TypeInt => self.out.push_str("int_t"),
            Node::TypeListRule => self.out.push_str("list_rule_t"),
            Node::TypeListStr => self.out.push_str("list_str_t"),
            Node::TypeListInt => self.out.push_str("list_int_t"),
            Node::TypeListListRule => self.out.push_str("list_list_rule_t"),
            Node::TypeListListStr => self.out.push_str("list_list_str_t"),
            Node::TypeListListInt => self.out.push_str("list_list_int_t"),
            Node::TypeVoid => self.out.push_str("void_t"),
            Node::TypeSpread => self.out.push_str("spread_t"),
            _ => self.out.push_str(self.text(self.ast.span(id))),
        }
    }

    fn format_expr_to_string(&mut self, id: NodeId) -> String {
        let saved = std::mem::take(&mut self.out);
        let was_dry = self.dry_run;
        self.dry_run = true;
        self.format_expr(id);
        self.dry_run = was_dry;
        std::mem::replace(&mut self.out, saved)
    }

    fn format_unary(&mut self, name: &str, inner: NodeId) {
        self.out.push_str(name);
        self.out.push('(');
        self.format_expr(inner);
        self.out.push(')');
    }

    fn format_binary(&mut self, name: &str, left: NodeId, right: NodeId) {
        self.out.push_str(name);
        self.out.push('(');
        self.format_expr(left);
        self.out.push_str(", ");
        self.format_expr(right);
        self.out.push(')');
    }

    fn format_variadic(&mut self, name: &str, range: ast::ChildRange) {
        let children = self.ast.child_slice(range);
        self.out.push_str(name);
        self.format_call_args(children);
    }

    fn format_call_args(&mut self, children: &[NodeId]) {
        // Check if comments force multiline.
        let has_inner_comments =
            if let (Some(&first), Some(&last)) = (children.first(), children.last()) {
                self.comments
                    .has_comments_in(self.ast.span(first).start, self.ast.span(last).end)
            } else {
                false
            };

        let force_multi = has_inner_comments;
        let single = if force_multi {
            String::new()
        } else {
            self.format_args_single_line(children)
        };
        let prefix_len = self.current_line_len();
        self.out.push('(');
        if !force_multi && prefix_len + single.len() + 2 <= self.config.max_line_width {
            self.out.push_str(&single);
        } else {
            self.out.push('\n');
            self.indent += 1;
            for (i, &child) in children.iter().enumerate() {
                let child_span = self.ast.span(child);
                self.emit_leading_comments(child_span.start);
                self.write_indent();
                self.format_expr(child);
                if i + 1 < children.len() || self.config.trailing_commas {
                    self.out.push(',');
                }
                self.emit_trailing_comment(child_span.end);
                self.out.push('\n');
            }
            self.indent -= 1;
            self.write_indent();
        }
        self.out.push(')');
    }

    fn format_args_single_line(&mut self, children: &[NodeId]) -> String {
        let mut parts = Vec::new();
        for &child in children {
            parts.push(self.format_expr_to_string(child));
        }
        parts.join(", ")
    }

    fn format_bracketed(&mut self, open: char, close: char, children: &[NodeId]) {
        let has_inner_comments =
            if let (Some(&first), Some(&last)) = (children.first(), children.last()) {
                self.comments
                    .has_comments_in(self.ast.span(first).start, self.ast.span(last).end)
            } else {
                false
            };

        let force_multi = has_inner_comments;
        let single = if force_multi {
            String::new()
        } else {
            self.format_args_single_line(children)
        };
        let prefix_len = self.current_line_len();
        self.out.push(open);
        if !force_multi && prefix_len + single.len() + 2 <= self.config.max_line_width {
            self.out.push_str(&single);
        } else {
            self.out.push('\n');
            self.indent += 1;
            for (i, &child) in children.iter().enumerate() {
                let child_span = self.ast.span(child);
                self.emit_leading_comments(child_span.start);
                self.write_indent();
                self.format_expr(child);
                if i + 1 < children.len() || self.config.trailing_commas {
                    self.out.push(',');
                }
                self.emit_trailing_comment(child_span.end);
                self.out.push('\n');
            }
            self.indent -= 1;
            self.write_indent();
        }
        self.out.push(close);
    }

    fn format_object(&mut self, range: ast::ChildRange) {
        let fields = self.ast.context.get_object(range);

        // Check for comments inside the object.
        let has_inner_comments = if let (Some(first), Some(last)) = (fields.first(), fields.last())
        {
            self.comments.has_comments_in(first.0.start, last.0.end)
        } else {
            false
        };

        if !has_inner_comments {
            let single = self.format_object_single_line(fields);
            let prefix_len = self.current_line_len();
            if prefix_len + single.len() + 4 <= self.config.max_line_width {
                self.out.push_str("{ ");
                self.out.push_str(&single);
                self.out.push_str(" }");
                return;
            }
        }

        {
            // Pack multiple fields per line, wrapping at max_line_width.
            // Leading comments break the line and start a new one after.
            self.out.push_str("{\n");
            self.indent += 1;

            let mut need_indent = true;

            for (i, &(key_span, value_id)) in fields.iter().enumerate() {
                let is_last = i + 1 == fields.len();

                // Emit any leading comments for this field's key.
                let has_comment = !self.comments.leading_at(key_span.start).is_empty();
                if has_comment {
                    if !need_indent {
                        // We were mid-line packing - end the line.
                        self.out.push('\n');
                    }
                    self.emit_leading_comments(key_span.start);
                    need_indent = true;
                }

                let key = self.text(key_span).to_owned();
                let value = self.format_expr_to_string(value_id);
                let suffix = if is_last && !self.config.trailing_commas {
                    ""
                } else {
                    ","
                };
                let entry = format!("{key}: {value}{suffix}");

                if need_indent {
                    self.write_indent();
                    need_indent = false;
                } else {
                    let line_len = self.current_line_len();
                    if line_len + 1 + entry.len() > self.config.max_line_width {
                        self.out.push('\n');
                        self.write_indent();
                    } else {
                        self.out.push(' ');
                    }
                }

                self.out.push_str(&entry);
            }
            self.out.push('\n');
            self.indent -= 1;
            self.write_indent();
            self.out.push('}');
        }
    }

    fn format_object_single_line(&mut self, fields: &[(Span, NodeId)]) -> String {
        let mut parts = Vec::new();
        for &(key_span, value_id) in fields {
            let key = self.text(key_span);
            let value = self.format_expr_to_string(value_id);
            parts.push(format!("{key}: {value}"));
        }
        parts.join(", ")
    }

    fn format_for(&mut self, for_id: ast::ForId) {
        let config = self.ast.get_for(for_id);
        self.out.push_str("for (");
        for (i, &(binding_span, ty_id)) in config.bindings.iter().enumerate() {
            if i > 0 {
                self.out.push_str(", ");
            }
            let binding_name = self.text(binding_span).to_owned();
            let ty = self.type_str(ty_id).to_owned();
            self.out.push_str(&binding_name);
            self.out.push_str(": ");
            self.out.push_str(&ty);
        }
        self.out.push_str(") in ");
        self.format_expr(config.iterable);
        self.out.push_str(" {\n");
        self.indent += 1;
        self.write_indent();
        self.format_expr(config.body);
        self.out.push('\n');
        self.indent -= 1;
        self.write_indent();
        self.out.push('}');
    }

    fn type_str(&self, id: NodeId) -> &str {
        match self.ast.node(id) {
            ast::Node::TypeRule => "rule_t",
            ast::Node::TypeStr => "str_t",
            ast::Node::TypeInt => "int_t",
            ast::Node::TypeListRule => "list_rule_t",
            ast::Node::TypeListStr => "list_str_t",
            ast::Node::TypeListInt => "list_int_t",
            ast::Node::TypeListListRule => "list_list_rule_t",
            ast::Node::TypeListListStr => "list_list_str_t",
            ast::Node::TypeListListInt => "list_list_int_t",
            ast::Node::TypeVoid => "void_t",
            ast::Node::TypeSpread => "spread_t",
            _ => "?",
        }
    }

    fn text(&self, span: Span) -> &'a str {
        self.ast.text(span)
    }

    fn fits_on_line(&self, text: &str) -> bool {
        !text.contains('\n') && self.current_line_len() + text.len() <= self.config.max_line_width
    }

    fn current_line_len(&self) -> usize {
        self.out
            .rfind('\n')
            .map_or(self.out.len(), |i| self.out.len() - i - 1)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ItemKind {
    Fn,
    Let,
    Rule,
    Other,
}

fn item_kind(ast: &Ast<'_>, id: NodeId) -> ItemKind {
    match ast.node(id) {
        ast::Node::Fn(_) => ItemKind::Fn,
        ast::Node::Let { .. } => ItemKind::Let,
        ast::Node::Rule { .. } | ast::Node::OverrideRule { .. } => ItemKind::Rule,
        _ => ItemKind::Other,
    }
}
