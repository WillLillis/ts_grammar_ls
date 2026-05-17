//! AST -> Doc visitor. One method per `Node` variant, dispatched from
//! `expr` (for expression-position nodes) or `item` (for top-level decls).
//!
//! Wrap policy (v1): per-line for combinator calls / lists / object literals
//! / grammar config. Fill mode (pack many per line) is a TODO - see the
//! design doc; per-line is the simpler starting point and produces sensible
//! output for typical grammars.
//!
//! Trivia (line comments + blank-line markers) attached during the trivia
//! pass is emitted at the start of each node's first token and at the end
//! of each node's last token (trailing-on-same-line case).

use tree_sitter_generate::nativedsl::ast::{
    ConfigField, IdentKind, ModuleContext, Node, NodeId, RepeatKind, SharedAst, Span,
};
use tree_sitter_generate::nativedsl::Ty;

use super::doc::{DocArena, DocId};
use super::trivia::{TriviaItem, TriviaMap};

/// Visitor state. One `Printer` is built per `format()` call.
pub struct Printer<'a> {
    pub arena: &'a mut DocArena,
    pub shared: &'a SharedAst,
    pub ctx: &'a ModuleContext,
    pub trivia: &'a TriviaMap,
}

impl<'a> Printer<'a> {
    pub fn new(
        arena: &'a mut DocArena,
        shared: &'a SharedAst,
        ctx: &'a ModuleContext,
        trivia: &'a TriviaMap,
    ) -> Self {
        Self {
            arena,
            shared,
            ctx,
            trivia,
        }
    }

    /// Format the entire module. Returns the root Doc.
    pub fn module(&mut self) -> DocId {
        let items: Vec<NodeId> = self.ctx.root_items.iter().copied().collect();
        let mut parts: Vec<DocId> = Vec::new();
        let mut first_unit = true;
        let mut prev_was_off = false;
        let mut emitted_off_starts: Vec<u32> = Vec::new();
        for id in &items {
            let span = self.shared.arena.span(*id);
            if let Some(range) = self.find_off_range(span.start) {
                if emitted_off_starts.contains(&range.start) {
                    continue; // already emitted this off block; skip in-range items
                }
                if !first_unit {
                    let l1 = self.arena.line();
                    let l2 = self.arena.line();
                    parts.push(self.arena.concat(&[l1, l2]));
                }
                // Preserve the raw source verbatim, including its trailing
                // newline; that newline is what separates the off block from
                // the `// tsg-format: on` pragma in the next item's leading.
                let raw = &self.ctx.source[range.start as usize..range.end as usize];
                parts.push(self.arena.raw(raw));
                emitted_off_starts.push(range.start);
                first_unit = false;
                prev_was_off = true;
                continue;
            }
            // Skip the inter-item separator right after an off block; the
            // raw region already ended with a newline and the next item's
            // leading carries the `// tsg-format: on` pragma in its own
            // paragraph.
            if !first_unit && !prev_was_off {
                let l1 = self.arena.line();
                let l2 = self.arena.line();
                parts.push(self.arena.concat(&[l1, l2]));
            }
            parts.push(self.item(*id));
            first_unit = false;
            prev_was_off = false;
        }
        // Trailing trivia after the last token: emit each comment on its own
        // line. Strings have to land in the arena, but the Vec itself we walk
        // by reference.
        for item in self.trivia.tail.iter() {
            if let TriviaItem::Comment(s) = item {
                let nl = self.arena.line();
                let cmt = self.arena.text(s.as_str());
                parts.push(self.arena.concat(&[nl, cmt]));
            }
        }
        self.arena.concat(&parts)
    }

    /// Find the format-off range that contains `offset`, if any.
    fn find_off_range(&self, offset: u32) -> Option<std::ops::Range<u32>> {
        self.trivia
            .format_off_ranges
            .iter()
            .find(|r| r.start <= offset && offset < r.end)
            .cloned()
    }

    // -- Top-level items -------------------------------------------------

    fn item(&mut self, id: NodeId) -> DocId {
        let span = self.shared.arena.span(id);
        let leading = self.emit_leading(span.start);
        let body = self.item_body(id);
        let trailing = self.emit_trailing(span.end);
        self.arena.concat(&[leading, body, trailing])
    }

    fn item_body(&mut self, id: NodeId) -> DocId {
        match *self.shared.arena.get(id) {
            Node::Rule {
                is_override,
                name,
                body,
            } => self.rule_doc(is_override, name, body),
            Node::Let { name, ty, value } => self.let_doc(name, ty, value),
            Node::Macro(macro_id) => self.macro_doc(macro_id),
            Node::External { name } => {
                let kw = self.arena.text("external");
                let space = self.arena.text(" ");
                let n = self.arena.text(self.span_text(name));
                self.arena.concat(&[kw, space, n])
            }
            Node::Cfg { name, child } => self.cfg_doc(name, child),
            // Sentinel marker - the actual fields live on `ctx.grammar_config`.
            Node::Grammar => self.grammar_block(),
            _ => self.expr(id),
        }
    }

    fn rule_doc(&mut self, is_override: bool, name: Span, body: NodeId) -> DocId {
        let mut parts = Vec::new();
        if is_override {
            parts.push(self.arena.text("override "));
        }
        parts.push(self.arena.text("rule "));
        parts.push(self.arena.text(self.span_text(name)));
        parts.push(self.arena.text(" { "));
        parts.push(self.expr(body));
        parts.push(self.arena.text(" }"));
        // Group so a short body stays one line; long body breaks.
        let flat = self.arena.concat(&parts);
        // For now keep single-line rule shape. Multi-line rule wrap happens
        // via the body expr's own group decisions when it doesn't fit -
        // but for `rule X { ... }` we want the `{ ... }` to break to multi
        // line if the body is multi-line.
        self.group_rule_body(name, is_override, body)
            .unwrap_or(flat)
    }

    /// Try to emit `rule NAME { BODY }` as a single group that breaks the
    /// body across lines if it doesn't fit flat. Returns `None` if the
    /// caller should fall back to the flat layout (currently always returns
    /// `Some` - kept as `Option` for future fast paths).
    fn group_rule_body(&mut self, name: Span, is_override: bool, body: NodeId) -> Option<DocId> {
        let mut head = Vec::new();
        if is_override {
            head.push(self.arena.text("override "));
        }
        head.push(self.arena.text("rule "));
        head.push(self.arena.text(self.span_text(name)));
        head.push(self.arena.text(" {"));
        let head_doc = self.arena.concat(&head);

        let sl_open = self.arena.softline();
        let body_doc = self.expr(body);
        let indented = self.arena.concat(&[sl_open, body_doc]);
        let indented = self.arena.indent(indented);

        let sl_close = self.arena.softline();
        let close = self.arena.text("}");

        let full = self.arena.concat(&[head_doc, indented, sl_close, close]);
        Some(self.arena.group(full))
    }

    fn let_doc(
        &mut self,
        name: Span,
        ty: Option<Ty>,
        value: NodeId,
    ) -> DocId {
        let mut parts = Vec::new();
        parts.push(self.arena.text("let "));
        parts.push(self.arena.text(self.span_text(name)));
        if let Some(ty) = ty {
            parts.push(self.arena.text(": "));
            parts.push(self.arena.text(ty.to_string()));
        }
        parts.push(self.arena.text(" = "));
        parts.push(self.expr(value));
        let doc = self.arena.concat(&parts);
        self.arena.group(doc)
    }

    fn macro_doc(&mut self, macro_id: tree_sitter_generate::nativedsl::ast::MacroId) -> DocId {
        // `macro NAME(p1: T1, p2: T2) RETURN_TY { BODY }`
        let config = self.shared.pools.get_macro(macro_id);
        let mut parts = Vec::new();
        parts.push(self.arena.text("macro "));
        parts.push(self.arena.text(self.span_text(config.name)));
        parts.push(self.arena.text("("));
        // Params: try flat, fall back to per-line.
        let mut param_parts = Vec::new();
        for (i, param) in config.params.iter().enumerate() {
            if i > 0 {
                let comma = self.arena.text(",");
                let sl = self.arena.softline();
                param_parts.push(comma);
                param_parts.push(sl);
            }
            let pname = self.arena.text(self.span_text(param.name));
            let colon = self.arena.text(": ");
            let pty = self.arena.text(param.ty.to_string());
            param_parts.push(self.arena.concat(&[pname, colon, pty]));
        }
        if !config.params.is_empty() {
            let sb_open = self.arena.softbreak();
            let params = self.arena.concat(&param_parts);
            let nil = self.arena.nil();
            let trailing = self.arena.text(",");
            let tc = self.arena.if_broken(trailing, nil);
            let inner = self.arena.concat(&[sb_open, params, tc]);
            parts.push(self.arena.indent(inner));
            let sb_close = self.arena.softbreak();
            parts.push(sb_close);
        }
        parts.push(self.arena.text(") "));
        parts.push(self.arena.text(config.return_ty.to_string()));
        parts.push(self.arena.text(" { "));
        parts.push(self.expr(config.body));
        parts.push(self.arena.text(" }"));
        let doc = self.arena.concat(&parts);
        self.arena.group(doc)
    }

    fn cfg_doc(&mut self, name: Span, child: NodeId) -> DocId {
        let attr = self.arena.text(format!("#[cfg({})]", self.span_text(name)));
        let line = self.arena.line();
        let inner = self.item_body(child);
        self.arena.concat(&[attr, line, inner])
    }

    // -- Grammar config block --------------------------------------------

    fn grammar_block(&mut self) -> DocId {
        let cfg = self
            .ctx
            .grammar_config
            .as_ref()
            .expect("grammar_block called without grammar_config");
        let mut field_parts = Vec::new();
        if let Some(lang) = &cfg.language {
            let leading = self.arena.nil(); // TODO: leading on `language` (uncommon).
            let key = self.arena.text("language");
            let colon = self.arena.text(": \"");
            let val = self.arena.text(lang.clone());
            let close = self.arena.text("\"");
            field_parts.push(self.arena.concat(&[leading, key, colon, val, close]));
        }
        for (field, node_id) in cfg.node_fields() {
            let value_start = self.shared.arena.span(node_id).start;
            let key_start = self.find_key_start(value_start);
            let leading = self.emit_leading(key_start);
            let key = self.arena.text(config_field_name(field));
            let colon = self.arena.text(": ");
            let val = self.expr(node_id);
            field_parts.push(self.arena.concat(&[leading, key, colon, val]));
        }
        if field_parts.is_empty() {
            return self.arena.text("grammar { }");
        }
        let mut lines = Vec::new();
        for (i, fp) in field_parts.into_iter().enumerate() {
            if i > 0 {
                let nl = self.arena.line();
                lines.push(nl);
            }
            let comma = self.arena.text(",");
            lines.push(self.arena.concat(&[fp, comma]));
        }
        let inner = self.arena.concat(&lines);
        let nl_first = self.arena.line();
        let inner_with_open = self.arena.concat(&[nl_first, inner]);
        let indented = self.arena.indent(inner_with_open);
        let open = self.arena.text("grammar {");
        let nl_close = self.arena.line();
        let close = self.arena.text("}");
        self.arena.concat(&[open, indented, nl_close, close])
    }

    /// Walk back from a value's `span.start` past whitespace, `:`, more
    /// whitespace, then the key identifier itself, returning the byte offset
    /// of the key token's start. Used to look up leading trivia on grammar
    /// block field keys, which aren't tracked as AST nodes.
    fn find_key_start(&self, value_start: u32) -> u32 {
        let bytes = self.ctx.source.as_bytes();
        let mut i = value_start as usize;
        while i > 0 && bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        if i > 0 && bytes[i - 1] == b':' {
            i -= 1;
        }
        while i > 0 && bytes[i - 1].is_ascii_whitespace() {
            i -= 1;
        }
        while i > 0 && (bytes[i - 1].is_ascii_alphanumeric() || bytes[i - 1] == b'_') {
            i -= 1;
        }
        i as u32
    }

    // -- Expressions -----------------------------------------------------

    fn expr(&mut self, id: NodeId) -> DocId {
        match *self.shared.arena.get(id) {
            Node::StringLit => {
                // Parser strips quotes from the span (start.strip_quotes()).
                // Re-emit them around the content text.
                let content = self.span_text(self.shared.arena.span(id));
                self.arena.text(format!("\"{content}\""))
            }
            Node::RawStringLit { .. } => {
                self.arena.text(self.span_text(self.shared.arena.span(id)))
            }
            Node::IntLit(n) => self.arena.text(n.to_string()),
            Node::Ident(_) => self.arena.text(self.span_text(self.shared.arena.span(id))),
            Node::Blank => self.arena.text("blank"),
            Node::Neg(inner) => {
                let neg = self.arena.text("-");
                let body = self.expr(inner);
                self.arena.concat(&[neg, body])
            }
            Node::Call { name, args } => {
                let parent_end = self.shared.arena.span(id).end;
                self.call_doc(name, args, parent_end)
            }
            Node::QualifiedCall(range) => {
                let (obj, name, _) = self.shared.pools.get_qualified_call(range);
                let obj_doc = self.expr(obj);
                let cc = self.arena.text("::");
                let name_doc = self.expr(name);
                let args_start = range; // qualified call's args follow obj+name
                // get_qualified_call returns the args slice; build a call-like
                // wrapper around them.
                let (_, _, args) = self.shared.pools.get_qualified_call(args_start);
                let args_ids: Vec<NodeId> = args.to_vec();
                let parent_end = self.shared.arena.span(id).end;
                let args_doc = self.args_doc(&args_ids, parent_end);
                self.arena.concat(&[obj_doc, cc, name_doc, args_doc])
            }
            Node::FieldAccess { obj, field } => {
                let obj_doc = self.expr(obj);
                let dot = self.arena.text(".");
                let f = self.arena.text(self.span_text(field));
                self.arena.concat(&[obj_doc, dot, f])
            }
            Node::QualifiedAccess { obj, member } => {
                let obj_doc = self.expr(obj);
                let cc = self.arena.text("::");
                let m = self.arena.text(self.span_text(member));
                self.arena.concat(&[obj_doc, cc, m])
            }
            Node::SeqOrChoice { seq, range } => {
                let name = if seq { "seq" } else { "choice" };
                let kids: Vec<NodeId> = self.shared.pools.child_slice(range).to_vec();
                let parent_end = self.shared.arena.span(id).end;
                self.named_args_doc(name, &kids, parent_end)
            }
            Node::Concat(range) => {
                let kids: Vec<NodeId> = self.shared.pools.child_slice(range).to_vec();
                let parent_end = self.shared.arena.span(id).end;
                self.named_args_doc("concat", &kids, parent_end)
            }
            Node::Repeat { kind, inner } => {
                let name = match kind {
                    RepeatKind::ZeroOrMore => "repeat",
                    RepeatKind::OneOrMore => "repeat1",
                    RepeatKind::Optional => "optional",
                };
                let parent_end = self.shared.arena.span(id).end;
                self.named_args_doc(name, &[inner], parent_end)
            }
            Node::Field { name, content } => {
                let head = self.arena.text("field(");
                let n = self.arena.text(self.span_text(name));
                let comma = self.arena.text(", ");
                let c = self.expr(content);
                let close = self.arena.text(")");
                self.arena.concat(&[head, n, comma, c, close])
            }
            Node::Alias { content, target } => {
                let parent_end = self.shared.arena.span(id).end;
                self.named_args_doc("alias", &[content, target], parent_end)
            }
            Node::Token { immediate, inner } => {
                let name = if immediate { "token_immediate" } else { "token" };
                let parent_end = self.shared.arena.span(id).end;
                self.named_args_doc(name, &[inner], parent_end)
            }
            Node::Prec {
                kind,
                value,
                content,
            } => {
                use tree_sitter_generate::nativedsl::ast::PrecKind;
                let name = match kind {
                    PrecKind::Default => "prec",
                    PrecKind::Left => "prec_left",
                    PrecKind::Right => "prec_right",
                    PrecKind::Dynamic => "prec_dynamic",
                };
                let parent_end = self.shared.arena.span(id).end;
                self.named_args_doc(name, &[value, content], parent_end)
            }
            Node::Reserved { context, content } => {
                // Parser strips the surrounding quotes from `context` (it's a
                // string literal). Re-emit them.
                let head = self.arena.text("reserved(");
                let ctx = self.arena.text(format!("\"{}\"", self.span_text(context)));
                let comma = self.arena.text(", ");
                let c = self.expr(content);
                let close = self.arena.text(")");
                self.arena.concat(&[head, ctx, comma, c, close])
            }
            Node::DynRegex { pattern, flags } => {
                let parent_end = self.shared.arena.span(id).end;
                if let Some(flags) = flags {
                    self.named_args_doc("regexp", &[pattern, flags], parent_end)
                } else {
                    self.named_args_doc("regexp", &[pattern], parent_end)
                }
            }
            Node::ModuleRef { import, path, .. } => {
                // Parser strips the surrounding quotes from `path` (same as
                // StringLit). Re-emit the path as a string literal.
                let name = if import { "import" } else { "inherit" };
                let head = self.arena.text(format!("{name}("));
                let p = self.arena.text(format!("\"{}\"", self.span_text(path)));
                let close = self.arena.text(")");
                self.arena.concat(&[head, p, close])
            }
            Node::GrammarConfig { module, field } => {
                let head = self.arena.text("grammar_config(");
                let m = self.expr(module);
                let comma = self.arena.text(", ");
                let f = self.arena.text(config_field_name(field));
                let close = self.arena.text(")");
                self.arena.concat(&[head, m, comma, f, close])
            }
            Node::Append { left, right } => {
                let parent_end = self.shared.arena.span(id).end;
                self.named_args_doc("append", &[left, right], parent_end)
            }
            Node::For { for_id, body } => self.for_doc(for_id, body),
            Node::List(range) => {
                let kids: Vec<NodeId> = self.shared.pools.child_slice(range).to_vec();
                let parent_end = self.shared.arena.span(id).end;
                self.delimited_list("[", "]", &kids, parent_end)
            }
            Node::Tuple(range) => {
                let kids: Vec<NodeId> = self.shared.pools.child_slice(range).to_vec();
                let parent_end = self.shared.arena.span(id).end;
                self.delimited_list("(", ")", &kids, parent_end)
            }
            Node::Object(range) => {
                let parent_end = self.shared.arena.span(id).end;
                self.object_doc(range, parent_end)
            }
            Node::MacroParam { .. } | Node::ForBinding { .. } => {
                // Resolved bindings keep the source span pointing at the
                // identifier; emit it verbatim.
                self.arena.text(self.span_text(self.shared.arena.span(id)))
            }
            // Top-level shapes encountered in expression position shouldn't
            // happen but fall back to source. Use `raw` (not `text`) because
            // the source span may legitimately contain newlines.
            Node::Grammar
            | Node::Rule { .. }
            | Node::Let { .. }
            | Node::Macro(_)
            | Node::External { .. }
            | Node::Cfg { .. } => self.arena.raw(self.span_text(self.shared.arena.span(id))),
            Node::Unreachable => self.arena.text(""),
        }
    }

    /// `name(arg, arg, ...)` with per-line wrap when broken. `parent_end`
    /// is the parent node's `span.end` (exclusive upper bound for trivia
    /// lookups inside the arg list - excludes the trailing on the close
    /// paren itself).
    fn named_args_doc(&mut self, name: &str, args: &[NodeId], parent_end: u32) -> DocId {
        let head = self.arena.text(format!("{name}("));
        let body = self.args_inner(args, parent_end);
        let close = self.arena.text(")");
        let doc = self.arena.concat(&[head, body, close]);
        self.arena.group(doc)
    }

    /// Just the args portion of a call - `( ... )`.
    fn args_doc(&mut self, args: &[NodeId], parent_end: u32) -> DocId {
        let open = self.arena.text("(");
        let body = self.args_inner(args, parent_end);
        let close = self.arena.text(")");
        let doc = self.arena.concat(&[open, body, close]);
        self.arena.group(doc)
    }

    /// Body of a call's args: softbreak + indent(items) + trailing-if-broken
    /// + softbreak. No outer parens. Each arg's leading trivia is emitted
    /// before it; any trailing comment on the comma between two args (or on
    /// the arg itself) is queried by range and emitted in place.
    fn args_inner(&mut self, args: &[NodeId], parent_end: u32) -> DocId {
        if args.is_empty() {
            return self.arena.nil();
        }
        // Single-arg "hug": let the inner expression wrap inside our parens
        // without adding our own indent layer. Turns
        //     token(
        //         prec(10, seq(a, b)),
        //     )
        // into
        //     token(prec(10, seq(a, b)))
        // (and lets the inner call's own wrap, if it has one, bracket the
        // outer close). Skipped when the arg has its own leading or trailing
        // trivia, which the normal layout handles.
        if args.len() == 1 {
            let span = self.shared.arena.span(args[0]);
            let no_leading = self.trivia.leading(span.start).is_empty();
            let no_trailing = self.trivia.trailing_in(span.end, parent_end).is_none();
            if no_leading && no_trailing {
                return self.expr(args[0]);
            }
        }
        let sb_open = self.arena.softbreak();
        let mut item_parts: Vec<DocId> = Vec::new();
        for (i, a) in args.iter().enumerate() {
            let span = self.shared.arena.span(*a);
            if i > 0 {
                item_parts.push(self.arena.text(","));
                // Trailing comment between previous arg and this one (on the
                // previous arg itself or on the separating comma).
                let prev_end = self.shared.arena.span(args[i - 1]).end;
                if let Some(c) = self.trivia.trailing_in(prev_end, span.start) {
                    let s = c.to_owned();
                    item_parts.push(self.arena.text(format!(" {s}")));
                    item_parts.push(self.arena.line());
                } else {
                    item_parts.push(self.arena.softline());
                }
            }
            let leading = self.emit_leading(span.start);
            item_parts.push(leading);
            item_parts.push(self.expr(*a));
        }
        let items = self.arena.concat(&item_parts);
        // Trailing-if-broken comma after the final arg. If a same-line
        // comment trails the final arg or its (source) trailing comma, emit
        // the comma unconditionally + the comment, then a hard line OUTSIDE
        // the indent so the close punctuation lands at the outer column.
        let last_end = self.shared.arena.span(*args.last().unwrap()).end;
        let last_trail = self.trivia.trailing_in(last_end, parent_end).map(str::to_owned);
        if let Some(c) = last_trail {
            let comma = self.arena.text(",");
            let cmt = self.arena.text(format!(" {c}"));
            let inner = self.arena.concat(&[sb_open, items, comma, cmt]);
            let indented = self.arena.indent(inner);
            let line = self.arena.line();
            self.arena.concat(&[indented, line])
        } else {
            let nil = self.arena.nil();
            let trailing_text = self.arena.text(",");
            let tail = self.arena.if_broken(trailing_text, nil);
            let inner = self.arena.concat(&[sb_open, items, tail]);
            let indented = self.arena.indent(inner);
            let sb_close = self.arena.softbreak();
            self.arena.concat(&[indented, sb_close])
        }
    }

    /// `[a, b, c]` / `(a, b, c)` - pack multiple per line when broken
    /// (Fill mode). Flat keeps the literal compact: `[a, b, c]` with no
    /// inner padding, matching common DSL style for `extras`, `inline`,
    /// `externals`, etc.
    fn delimited_list(
        &mut self,
        open: &str,
        close: &str,
        items: &[NodeId],
        parent_end: u32,
    ) -> DocId {
        if items.is_empty() {
            return self.arena.text(format!("{open}{close}"));
        }
        // Entries: e1 "," softline e2 "," softline ... e_n if_broken(",", nil).
        // Softline separators let Fill (in the broken branch) pack multiple
        // entries per line; in the flat branch they render as plain spaces.
        let mut entry_parts: Vec<DocId> = Vec::new();
        for (i, &item) in items.iter().enumerate() {
            let span = self.shared.arena.span(item);
            if i > 0 {
                entry_parts.push(self.arena.text(","));
                let prev_end = self.shared.arena.span(items[i - 1]).end;
                if let Some(c) = self.trivia.trailing_in(prev_end, span.start) {
                    let s = c.to_owned();
                    entry_parts.push(self.arena.text(format!(" {s}")));
                    entry_parts.push(self.arena.line());
                } else {
                    entry_parts.push(self.arena.softline());
                }
            }
            let leading = self.emit_leading(span.start);
            entry_parts.push(leading);
            entry_parts.push(self.expr(item));
        }
        let nil = self.arena.nil();
        let tc_text = self.arena.text(",");
        let trailing_comma = self.arena.if_broken(tc_text, nil);
        entry_parts.push(trailing_comma);
        let entries = self.arena.concat(&entry_parts);

        let last_end = self.shared.arena.span(*items.last().unwrap()).end;
        let last_trail = self.trivia.trailing_in(last_end, parent_end).map(str::to_owned);

        let flat_body = entries;
        let broken_body = {
            let line_in = self.arena.line();
            let fill = self.arena.fill(entries);
            let inner = self.arena.concat(&[line_in, fill]);
            let indented = self.arena.indent(inner);
            let line_out = self.arena.line();
            if let Some(c) = last_trail {
                let trail_cmt = self.arena.text(format!(" {c}"));
                self.arena.concat(&[indented, trail_cmt, line_out])
            } else {
                self.arena.concat(&[indented, line_out])
            }
        };
        let body = self.arena.if_broken(broken_body, flat_body);
        let open_d = self.arena.text(open.to_string());
        let close_d = self.arena.text(close.to_string());
        let doc = self.arena.concat(&[open_d, body, close_d]);
        self.arena.group(doc)
    }

    fn object_doc(
        &mut self,
        range: tree_sitter_generate::nativedsl::ast::ChildRange,
        parent_end: u32,
    ) -> DocId {
        let fields: Vec<(Span, NodeId)> = self.shared.pools.get_object(range).to_vec();
        if fields.is_empty() {
            return self.arena.text("{}");
        }
        // Entries: e1 "," softline e2 "," softline ... e_n if_broken(",", nil)
        // Each separator is a softline so Fill packs entries per line. The
        // trailing comma is gated on the surrounding Group's break state.
        let mut entry_parts: Vec<DocId> = Vec::new();
        for (i, (key, value)) in fields.iter().enumerate() {
            if i > 0 {
                entry_parts.push(self.arena.text(","));
                let prev_end = self.shared.arena.span(fields[i - 1].1).end;
                if let Some(c) = self.trivia.trailing_in(prev_end, key.start) {
                    let s = c.to_owned();
                    entry_parts.push(self.arena.text(format!(" {s}")));
                    entry_parts.push(self.arena.line());
                } else {
                    entry_parts.push(self.arena.softline());
                }
            }
            let leading = self.emit_leading(key.start);
            entry_parts.push(leading);
            let k = self.arena.text(self.span_text(*key));
            let colon = self.arena.text(": ");
            let v = self.expr(*value);
            entry_parts.push(self.arena.concat(&[k, colon, v]));
        }
        let nil = self.arena.nil();
        let tc_text = self.arena.text(",");
        let trailing_comma = self.arena.if_broken(tc_text, nil);
        entry_parts.push(trailing_comma);
        let entries = self.arena.concat(&entry_parts);

        // Flat: `{ e1, e2 }`. Broken: `{<indent \n Fill(entries)> \n }`. A
        // same-line comment trailing the final entry forces break and adds
        // ` // comment` between Fill and the closing line.
        let last_end = self.shared.arena.span(fields.last().unwrap().1).end;
        let last_trail = self.trivia.trailing_in(last_end, parent_end).map(str::to_owned);

        let flat_body = {
            let sp1 = self.arena.text(" ");
            let sp2 = self.arena.text(" ");
            self.arena.concat(&[sp1, entries, sp2])
        };
        let broken_body = {
            let line_in = self.arena.line();
            let fill = self.arena.fill(entries);
            let inner = self.arena.concat(&[line_in, fill]);
            let indented = self.arena.indent(inner);
            let line_out = self.arena.line();
            if let Some(c) = last_trail {
                let trail_cmt = self.arena.text(format!(" {c}"));
                self.arena.concat(&[indented, trail_cmt, line_out])
            } else {
                self.arena.concat(&[indented, line_out])
            }
        };
        let body = self.arena.if_broken(broken_body, flat_body);
        let open = self.arena.text("{");
        let close = self.arena.text("}");
        let doc = self.arena.concat(&[open, body, close]);
        self.arena.group(doc)
    }


    fn for_doc(&mut self, for_id: tree_sitter_generate::nativedsl::ast::ForId, body: NodeId) -> DocId {
        let cfg = self.shared.pools.get_for(for_id);
        // `for (b1, b2, ...) in iterable { body }`. Parens around the
        // bindings are required by the parser (even for a single binding).
        let head = self.arena.text("for (");
        let mut binding_parts = Vec::new();
        for (i, b) in cfg.bindings.iter().enumerate() {
            if i > 0 {
                let comma = self.arena.text(", ");
                binding_parts.push(comma);
            }
            let name = self.arena.text(self.span_text(b.name));
            let colon = self.arena.text(": ");
            let ty = self.arena.text(b.ty.to_string());
            binding_parts.push(self.arena.concat(&[name, colon, ty]));
        }
        let bindings = self.arena.concat(&binding_parts);
        let kw_in = self.arena.text(") in ");
        let iterable = self.expr(cfg.iterable);
        let space_brace = self.arena.text(" { ");
        let body_doc = self.expr(body);
        let close = self.arena.text(" }");
        let doc = self.arena.concat(&[head, bindings, kw_in, iterable, space_brace, body_doc, close]);
        self.arena.group(doc)
    }

    fn call_doc(
        &mut self,
        name: NodeId,
        args: tree_sitter_generate::nativedsl::ast::ChildRange,
        parent_end: u32,
    ) -> DocId {
        let name_doc = self.expr(name);
        let args_slice: Vec<NodeId> = self.shared.pools.child_slice(args).to_vec();
        let args_doc = self.args_doc(&args_slice, parent_end);
        self.arena.concat(&[name_doc, args_doc])
    }

    // -- Trivia emission -------------------------------------------------

    fn emit_leading(&mut self, offset: u32) -> DocId {
        let items = self.trivia.leading(offset);
        // Collect comments and inter-comment blank lines. Leading BlankLine
        // markers with no following comment are dropped here - top-level
        // separators are emitted by `module()`, so re-emitting them would
        // double up.
        let mut parts = Vec::new();
        let mut emitted_anything = false;
        for item in items {
            match item {
                TriviaItem::Comment(s) => {
                    if emitted_anything {
                        let nl = self.arena.line();
                        parts.push(nl);
                    }
                    parts.push(self.arena.text(s.clone()));
                    emitted_anything = true;
                }
                TriviaItem::BlankLine if emitted_anything => {
                    // Blank gap *between* comment groups: extra newline.
                    let nl = self.arena.line();
                    parts.push(nl);
                }
                TriviaItem::BlankLine => {
                    // Leading blank line before any comment - skip; the
                    // surrounding separator handles spacing.
                }
            }
        }
        if !emitted_anything {
            return self.arena.nil();
        }
        // After all comments, drop a Line so the next content starts fresh.
        let final_line = self.arena.line();
        parts.push(final_line);
        self.arena.concat(&parts)
    }

    fn emit_trailing(&mut self, offset: u32) -> DocId {
        if let Some(s) = self.trivia.trailing(offset) {
            let space = self.arena.text(" ");
            let text = self.arena.text(s.to_owned());
            self.arena.concat(&[space, text])
        } else {
            self.arena.nil()
        }
    }

    // -- Utility ---------------------------------------------------------

    fn span_text(&self, span: Span) -> &'a str {
        let s = self.ctx.source.as_str();
        &s[span.start as usize..span.end as usize]
    }
}

fn config_field_name(f: ConfigField) -> &'static str {
    match f {
        ConfigField::Language => "language",
        ConfigField::Inherits => "inherits",
        ConfigField::Extras => "extras",
        ConfigField::Externals => "externals",
        ConfigField::Supertypes => "supertypes",
        ConfigField::Inline => "inline",
        ConfigField::Word => "word",
        ConfigField::Conflicts => "conflicts",
        ConfigField::Precedences => "precedences",
        ConfigField::Reserved => "reserved",
        ConfigField::Start => "start",
        ConfigField::Flags => "flags",
    }
}

// `IdentKind` re-exported here so callers don't need the AST module in scope.
#[allow(dead_code)]
fn _ident_kind_marker(_k: IdentKind) {}
