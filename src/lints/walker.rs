//! Shared AST traversal helpers for lints.
//!
//! Tree-sitter's `Node` enum doesn't expose a child enumerator, and the
//! shape varies enough that each lint would need its own `push_children`
//! match - which we discovered after the second copy. This module is the
//! one place that knows how to enumerate children for every variant.
//!
//! NOTE: `apply_cfg::walk_children` upstream has nearly the same shape.
//! A natural upstream addition would be `Node::children(&self, &AstPools)`
//! returning an iterator over child `NodeId`s, removing this whole file
//! and that one. Defer until a third upstream consumer makes the case.

use tree_sitter_generate::nativedsl::ast::{Node, NodeId, SharedAst};

/// Push every child `NodeId` of `node` onto `stack`. Used by lints that
/// do depth-first or breadth-first traversal of a module's AST.
///
/// Variants that own pool-backed children (`Macro` body, `Object` field
/// values, etc.) are included so the walk is exhaustive.
pub fn push_children(node: &Node, shared: &SharedAst, stack: &mut Vec<NodeId>) {
    match *node {
        Node::SeqOrChoice { range, .. }
        | Node::List(range)
        | Node::Concat(range)
        | Node::Tuple(range)
        | Node::RuleSet(range) => {
            for i in range.as_range() {
                stack.push(shared.pools.children[i]);
            }
        }
        Node::Object(range) => {
            for i in range.as_range() {
                stack.push(shared.pools.object_fields[i].value);
            }
        }
        // `ExpandedRule` carries its body out-of-line in the expansion table.
        Node::ExpandedRule(expand_id) => {
            stack.push(shared.pools.get_expansion(expand_id).body);
        }
        Node::Call { name, args } => {
            stack.push(name);
            for i in args.as_range() {
                stack.push(shared.pools.children[i]);
            }
        }
        Node::Rule { body: c, .. }
        | Node::Let { value: c, .. }
        | Node::Repeat { inner: c, .. }
        | Node::Token { inner: c, .. }
        | Node::Field { content: c, .. }
        | Node::Reserved { content: c, .. }
        | Node::FieldAccess { obj: c, .. }
        | Node::Neg(c)
        | Node::QualifiedAccess { obj: c, .. }
        | Node::GrammarConfig { module: c, .. }
        | Node::SymRef { expr: c }
        | Node::For { body: c, .. }
        | Node::Cfg { child: c, .. } => stack.push(c),
        Node::ComputedRule {
            name_expr, body, ..
        } => {
            stack.push(name_expr);
            stack.push(body);
        }
        Node::Alias {
            content: a,
            target: b,
        }
        | Node::Append { left: a, right: b }
        | Node::BinOp { lhs: a, rhs: b, .. }
        | Node::Prec {
            value: a,
            content: b,
            ..
        } => {
            stack.push(a);
            stack.push(b);
        }
        Node::DynRegex { pattern, flags } => {
            stack.push(pattern);
            if let Some(f) = flags {
                stack.push(f);
            }
        }
        Node::Macro(macro_id) => {
            stack.push(shared.pools.get_macro(macro_id).body);
        }
        Node::Grammar
        | Node::Forward { .. }
        | Node::StringLit(_)
        | Node::IntLit(_)
        | Node::Ident(_)
        | Node::Blank
        | Node::Eof
        | Node::Import { .. }
        | Node::Inherit { .. }
        | Node::MacroParam { .. }
        | Node::ForBinding { .. }
        // Resolved cross-module rule reference; carries indices, not child nodes.
        | Node::ModuleRule { .. }
        | Node::Unreachable => {}
    }
}
