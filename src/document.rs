use ropey::Rope;
use tower_lsp::lsp_types::Diagnostic;

use tree_sitter_generate::nativedsl::ast::Span;
use tree_sitter_generate::nativedsl::lexer::Token;

/// Cached results from the two diagnostic phases.
#[derive(Default, Clone)]
pub struct DiagnosticCache {
    /// Diagnostics from the DSL evaluation pipeline
    pub dsl: Vec<Diagnostic>,
    /// Diagnostics from the full generate pipeline
    pub generate: Vec<Diagnostic>,
}

impl DiagnosticCache {
    /// All diagnostics merged for publishing.
    #[must_use]
    pub fn all(&self) -> Vec<Diagnostic> {
        let mut out = self.dsl.clone();
        out.extend(self.generate.iter().cloned());
        out
    }
}

/// A definition extracted from the AST.
#[derive(Clone, Debug)]
pub struct Definition {
    pub name: String,
    pub kind: DefKind,
    pub name_span: Span,
    pub full_span: Span,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DefKind {
    Rule,
    OverrideRule,
    Function {
        signature: String,
    },
    /// The span of the enclosing function, if this is a parameter or local.
    /// `None` for top-level definitions.
    Let {
        scope: Option<Span>,
    },
    /// A key in an object literal (e.g. `ADD` in `{ ADD: 1 }`).
    ObjectKey,
    /// A function parameter.
    Parameter {
        scope: Span,
    },
}

impl DefKind {
    #[must_use]
    pub const fn label(&self) -> &'static str {
        match self {
            Self::Rule => "rule",
            Self::OverrideRule => "override rule",
            Self::Function { .. } => "fn",
            Self::Let { .. } => "let",
            Self::ObjectKey => "field",
            Self::Parameter { .. } => "parameter",
        }
    }

    #[must_use]
    pub const fn scope(&self) -> Option<Span> {
        match self {
            Self::Let { scope } => *scope,
            Self::Parameter { scope } => Some(*scope),
            Self::Rule | Self::OverrideRule | Self::Function { .. } | Self::ObjectKey => None,
        }
    }
}

/// An identifier reference extracted from the resolved AST.
#[derive(Clone, Debug)]
pub struct Reference {
    pub span: Span,
    pub kind: RefKind,
    /// The span of the enclosing function, if this reference is inside one.
    /// `None` for top-level references.
    pub scope: Option<Span>,
}

#[derive(Clone, Debug)]
pub enum RefKind {
    Rule(String),
    /// A rule reference via `base::rule_name` (inlined from base grammar).
    BaseRule(String),
    Variable(String),
    /// The field part of `obj.field` access (e.g. `CALL` in `PREC.CALL`).
    ObjectField {
        field: String,
        object: String,
    },
    /// The path argument to `inherit("path")`.
    InheritPath,
    /// A builtin combinator keyword (e.g. `seq`, `choice`, `repeat`).
    Builtin,
}

impl Reference {
    /// Check if this reference matches a given word, using the source text
    /// to resolve builtin names from spans. If `cursor_scope` is provided,
    /// scoped references (inside functions) only match if they share the
    /// same scope.
    #[must_use]
    pub fn matches_word(&self, word: &str, source: &str, cursor_scope: Option<Span>) -> bool {
        let name_matches = match &self.kind {
            RefKind::Rule(name) | RefKind::BaseRule(name) | RefKind::Variable(name) => name == word,
            RefKind::ObjectField { field, .. } => field == word,
            RefKind::Builtin => {
                let s = self.span;
                &source[s.start as usize..s.end as usize] == word
            }
            RefKind::InheritPath => false,
        };
        if !name_matches {
            return false;
        }
        // If the cursor is in a specific scope and this reference is also
        // scoped, they must match.
        match (cursor_scope, self.scope) {
            (Some(cs), Some(rs)) => cs.start == rs.start && cs.end == rs.end,
            _ => true,
        }
    }
}

/// On-demand analysis results from a pipeline run.
///
/// Each field is populated as far as the pipeline gets. If lex fails, all
/// fields are `None`. If parse succeeds but resolve fails, `tokens`/`grammar_span`/
/// `definitions` are `Some`, references may be partial, etc.
///
/// Type information (variable types, object fields) is NOT included here. The
/// `Ast` and `TypeEnv` borrow the source text (`&'src str`), making them
/// impossible to store alongside the owned `Document.text` without
/// self-referential structs. Instead, features that need type info (e.g. hover
/// on let bindings) re-run the pipeline on demand via `with_type_env` - this
/// is <1ms even for large grammars, so the cost is negligible.
#[derive(Default, Clone)]
pub struct Analysis {
    /// Lexer tokens. Available after a successful lex.
    pub tokens: Option<Vec<Token>>,
    /// Span of the grammar block (if present). Available after parse.
    pub grammar_span: Option<Span>,
    /// Definitions extracted from the AST. Available after parse.
    pub definitions: Option<Vec<Definition>>,
    /// References extracted from the resolved AST. Available after resolve.
    pub references: Option<Vec<Reference>>,
    /// Resolved absolute path to the base grammar. Available after stage 4.
    pub base_grammar_path: Option<std::path::PathBuf>,
    /// Definitions from the base grammar (for go-to-def on `base::foo`).
    pub base_definitions: Option<Vec<Definition>>,
    /// References from the base grammar (for find-references on `base::foo`).
    pub base_references: Option<Vec<Reference>>,
    /// Cached rope for the base grammar source (for span-to-range conversion).
    pub base_rope: Option<Rope>,
}

impl Analysis {
    /// Find the narrowest enclosing scope for a given byte offset.
    /// Checks both function spans and for-loop scopes from parameter definitions.
    #[must_use]
    pub fn scope_at(&self, offset: u32) -> Option<Span> {
        let definitions = self.definitions.as_ref()?;
        let mut best: Option<Span> = None;
        for def in definitions {
            let scope_span = match def.kind {
                DefKind::Function { .. } => def.full_span,
                // The scope field of a parameter points to its enclosing scope.
                DefKind::Parameter { scope } => scope,
                _ => continue,
            };
            if offset >= scope_span.start && offset < scope_span.end {
                match best {
                    Some(b) if scope_span.start > b.start => best = Some(scope_span),
                    None => best = Some(scope_span),
                    _ => {}
                }
            }
        }
        best
    }

    /// Classify what kind of identifier the cursor is on, based on token context.
    /// Use this to route handler logic uniformly across hover/references/highlight.
    #[must_use]
    pub fn cursor_context(&self, offset: u32) -> CursorContext {
        let Some(tokens) = self.tokens.as_deref() else {
            return CursorContext::Identifier {
                scope: self.scope_at(offset),
            };
        };
        if let Some(gs) = self.grammar_span
            && crate::text::is_grammar_config_field(tokens, gs, offset)
        {
            return CursorContext::GrammarConfigField;
        }
        if crate::text::is_base_rule_access(tokens, offset) {
            return CursorContext::BaseRuleAccess;
        }
        CursorContext::Identifier {
            scope: self.scope_at(offset),
        }
    }
}

/// Classification of an identifier at the cursor position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorContext {
    /// Cursor is on a grammar config field (e.g. `reserved:` inside the grammar block).
    /// Not referenceable; not highlightable.
    GrammarConfigField,
    /// Cursor is on the rule part of `base::rule_name`.
    /// References/highlights should look at base grammar usages, not local overrides.
    BaseRuleAccess,
    /// Cursor is on a regular identifier (rule, fn, let, parameter, etc.).
    Identifier { scope: Option<Span> },
}

/// State for an open document.
pub struct Document {
    /// The full source text.
    pub text: String,
    /// Rope for efficient position/offset conversion.
    pub rope: Rope,
    /// LSP document version.
    pub version: i32,
    /// Cached diagnostics, split by phase.
    pub diagnostics: DiagnosticCache,
}
