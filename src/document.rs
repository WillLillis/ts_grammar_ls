use std::path::PathBuf;

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
    /// An import binding (e.g. `helpers` in `let helpers = import("helpers.tsg")`).
    Import,
    /// An inherit binding (e.g. `base` in `let base = inherit("base.tsg")`).
    Inherit,
    /// An `external <name>` declaration: forward-declares an externally-
    /// provided symbol (typically scanner-emitted) accessible via qualified
    /// access from importers.
    External,
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
            Self::Function { .. } => "macro",
            Self::Let { .. } => "let",
            Self::Import => "import",
            Self::Inherit => "inherit",
            Self::External => "external",
            Self::ObjectKey => "field",
            Self::Parameter { .. } => "parameter",
        }
    }

    #[must_use]
    pub const fn scope(&self) -> Option<Span> {
        match self {
            Self::Let { scope } => *scope,
            Self::Parameter { scope } => Some(*scope),
            Self::Rule
            | Self::OverrideRule
            | Self::Function { .. }
            | Self::Import
            | Self::Inherit
            | Self::External
            | Self::ObjectKey => None,
        }
    }

    /// Check if this definition is visible from the given cursor scope.
    /// Top-level definitions (scope = None) are visible everywhere.
    /// Scoped definitions (parameters, locals) are visible if the
    /// definition's scope contains the cursor scope (handles nesting).
    #[must_use]
    pub const fn visible_from(&self, cursor_scope: Option<Span>) -> bool {
        match (cursor_scope, self.scope()) {
            // Cursor is inside a scope, def is scoped - visible if
            // the def's scope encloses the cursor's scope.
            (Some(cs), Some(ds)) => ds.start <= cs.start && cs.end <= ds.end,
            // Top-level defs are visible from any scope.
            (_, None) => true,
            // Scoped defs are not visible from top level.
            (None, Some(_)) => false,
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
    /// The path argument to `import("path")`.
    ImportPath,
    /// A member accessed through an imported module (e.g. `fn_name` in `mod::fn_name(args)`).
    /// For nested access like `a::b::c`, path is `["a", "b"]` and member is `"c"`.
    ImportedMember {
        path: Vec<String>,
        member: String,
    },
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
            RefKind::ImportedMember { member, .. } => member == word,
            RefKind::Builtin => {
                let s = self.span;
                &source[s.start as usize..s.end as usize] == word
            }
            RefKind::InheritPath | RefKind::ImportPath => false,
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

/// Cached info about an external module (inherited or imported), for IDE features.
#[derive(Clone)]
pub struct ExternalModuleInfo {
    pub path: PathBuf,
    pub definitions: Vec<Definition>,
    pub references: Vec<Reference>,
    pub rope: Rope,
    /// Sub-imports within this module, for resolving nested `a::b::c` access.
    pub import_modules: Vec<(String, Self)>,
}

impl ExternalModuleInfo {
    /// Look up a nested sub-module by variable name.
    #[must_use]
    pub fn get_submodule(&self, name: &str) -> Option<&Self> {
        self.import_modules
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, info)| info)
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
    /// The source text this analysis was computed from. Stored here so
    /// handlers always use text that matches the token/definition spans,
    /// even when the document has been edited since (stale analysis reuse).
    pub source: String,
    /// Rope for the source text, for position/offset conversion.
    pub rope: Rope,
    /// Lexer tokens. Available after a successful lex.
    pub tokens: Option<Vec<Token>>,
    /// Span of the grammar block (if present). Available after parse.
    pub grammar_span: Option<Span>,
    /// Definitions extracted from the AST. Available after parse.
    pub definitions: Option<Vec<Definition>>,
    /// References extracted from the resolved AST. Available after resolve.
    pub references: Option<Vec<Reference>>,
    /// Parsed info from the inherited base grammar (for go-to-def, references,
    /// and completion on `base::rule_name`). Available after stage 4.
    pub base_module: Option<ExternalModuleInfo>,
    /// Imported modules, paired with their let-binding name (e.g. `"helpers"` for
    /// `let helpers = import("helpers.tsg")`).
    pub import_modules: Vec<(String, ExternalModuleInfo)>,
}

impl Analysis {
    /// Find the reference at `offset`, preferring more specific kinds when
    /// multiple references share a span (e.g. `helpers::commaSep` produces
    /// both an inner `Variable("commaSep")` and an outer `ImportedMember`).
    /// `BaseRule`/`ImportedMember`/`ImportedPath`/`InheritPath`/`ObjectField`
    /// win over plain `Rule`/`Variable`.
    #[must_use]
    pub fn reference_at(&self, offset: u32) -> Option<&Reference> {
        fn priority(kind: &RefKind) -> u8 {
            match kind {
                RefKind::BaseRule(_)
                | RefKind::ImportedMember { .. }
                | RefKind::ImportPath
                | RefKind::InheritPath
                | RefKind::ObjectField { .. } => 0,
                _ => 1,
            }
        }
        self.references
            .iter()
            .flatten()
            .filter(|r| offset >= r.span.start && offset < r.span.end)
            .min_by_key(|r| priority(&r.kind))
    }

    /// Resolve `name` to the definition it refers to from a given enclosing
    /// scope, using lexical scoping. Among definitions whose name matches and
    /// are visible from `scope`, the one with the smallest (innermost) scope
    /// wins. Top-level definitions are visible everywhere but lose to any
    /// in-scope binding of the same name.
    #[must_use]
    pub fn binding_for(&self, name: &str, scope: Option<Span>) -> Option<&Definition> {
        self.definitions
            .as_ref()?
            .iter()
            .filter(|d| d.name == name && d.kind.visible_from(scope))
            .min_by_key(|d| match d.kind.scope() {
                Some(s) => (0u8, s.end - s.start),
                None => (1, 0),
            })
    }

    /// Walk a qualified-access chain to the leaf module. For `a::b::c`, given
    /// `path = ["a", "b"]`, returns the `ExternalModuleInfo` for `b` (a's
    /// sub-import). The first segment must be a top-level binding (import or
    /// inherit); subsequent segments walk through nested sub-imports.
    #[must_use]
    pub fn resolve_import_chain(&self, path: &[String]) -> Option<&ExternalModuleInfo> {
        let first = path.first()?;
        let mut module_info = self.get_module(first.as_str())?;
        for segment in &path[1..] {
            module_info = module_info.get_submodule(segment)?;
        }
        Some(module_info)
    }

    /// Look up a module by its variable name. Checks both imported modules
    /// and the inherited base grammar.
    #[must_use]
    pub fn get_module(&self, name: &str) -> Option<&ExternalModuleInfo> {
        // Check imports.
        if let Some((_, info)) = self.import_modules.iter().find(|(n, _)| n == name) {
            return Some(info);
        }
        // Check if this name is the inherit binding.
        if let Some(base) = &self.base_module
            && self.definitions.as_ref().is_some_and(|defs| {
                defs.iter()
                    .any(|d| d.name == name && d.kind == DefKind::Inherit)
            })
        {
            return Some(base);
        }
        None
    }

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
    pub fn cursor_context(&self, offset: u32, source: &str) -> CursorContext {
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
            // Distinguish base rule access from import module access by checking
            // whether the qualifier is an import definition.
            if let Some(qualifier) = crate::text::qualified_access_module(tokens, source, offset)
                && self.definitions.as_ref().is_some_and(|defs| {
                    defs.iter()
                        .any(|d| d.name == qualifier && d.kind == DefKind::Import)
                })
            {
                return CursorContext::ImportModuleAccess {
                    scope: self.scope_at(offset),
                };
            }
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
    /// Cursor is on a member accessed through an imported module (`mod::fn_name`).
    ImportModuleAccess { scope: Option<Span> },
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
    /// Last analysis where parse succeeded. Used as a fallback when the
    /// current text fails to parse, so handlers (hover, completion, ...) keep
    /// working mid-keystroke. Never consulted on the success path: every
    /// `get_analysis` re-runs analyze and serves the fresh result if it
    /// parsed.
    pub last_good_analysis: Option<std::sync::Arc<Analysis>>,
    /// Canonical paths of external files (inherits + transitive imports) this
    /// document's last successful analysis loaded. Maintained alongside the
    /// `Backend.dependents` reverse index; on update we diff against the new
    /// dep set to keep both sides consistent without iterating the whole map.
    pub deps: Vec<PathBuf>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_from_top_level_always_visible() {
        let rule = DefKind::Rule;
        assert!(rule.visible_from(None));
        assert!(rule.visible_from(Some(Span::new(10, 50))));
    }

    #[test]
    fn visible_from_scoped_not_visible_at_top_level() {
        let param = DefKind::Parameter {
            scope: Span::new(10, 50),
        };
        assert!(!param.visible_from(None));
    }

    #[test]
    fn visible_from_same_scope() {
        let param = DefKind::Parameter {
            scope: Span::new(10, 50),
        };
        assert!(param.visible_from(Some(Span::new(10, 50))));
    }

    #[test]
    fn visible_from_inner_scope() {
        // Param defined in outer scope [10, 100], cursor in inner scope [20, 50].
        let param = DefKind::Parameter {
            scope: Span::new(10, 100),
        };
        assert!(param.visible_from(Some(Span::new(20, 50))));
    }

    #[test]
    fn visible_from_outer_scope_not_visible() {
        // Param defined in inner scope [20, 50], cursor in outer scope [10, 100].
        let param = DefKind::Parameter {
            scope: Span::new(20, 50),
        };
        assert!(!param.visible_from(Some(Span::new(10, 100))));
    }

    #[test]
    fn visible_from_disjoint_scope() {
        let param = DefKind::Parameter {
            scope: Span::new(10, 50),
        };
        assert!(!param.visible_from(Some(Span::new(60, 100))));
    }
}
