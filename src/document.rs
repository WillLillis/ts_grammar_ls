use std::path::PathBuf;

use ropey::Rope;
use tower_lsp::lsp_types::Diagnostic;

use std::sync::Arc;

use tree_sitter_generate::nativedsl::ast::{SharedAst, Span};
use tree_sitter_generate::nativedsl::lexer::Token;
use tree_sitter_generate::nativedsl::typecheck::Ty;

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
    /// A `let` binding. `scope` is the span of the enclosing function for
    /// scoped lets, `None` for top-level. `ty` is the inferred type when the
    /// loader pipeline succeeded, `None` otherwise (e.g. mid-keystroke
    /// states where typecheck failed).
    Let {
        scope: Option<Span>,
        ty: Option<Ty>,
    },
    /// An import binding (e.g. `helpers` in `let helpers = import("helpers.tsg")`).
    Import,
    /// An inherit binding (e.g. `base` in `let base = inherit("base.tsg")`).
    Inherit,
    /// An `external <name>` declaration: forward-declares an externally-
    /// provided symbol (typically scanner-emitted) accessible via qualified
    /// access from importers.
    External,
    /// A key in an object literal (e.g. `ADD` in `{ ADD: 1 }`). `value_span`
    /// covers the right-hand side of the field for source-text display.
    ObjectKey { value_span: Span },
    /// A function parameter or for-loop binding. `scope` is the span of the
    /// owning macro / for-loop. `ty` is the declared type (parameters always
    /// have an explicit annotation; for-loop bindings inherit from the
    /// iterable's element type).
    Parameter {
        scope: Span,
        ty: Ty,
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
            Self::ObjectKey { .. } => "field",
            Self::Parameter { .. } => "parameter",
        }
    }

    #[must_use]
    pub const fn scope(&self) -> Option<Span> {
        match self {
            Self::Let { scope, .. } => *scope,
            Self::Parameter { scope, .. } => Some(*scope),
            Self::Rule
            | Self::OverrideRule
            | Self::Function { .. }
            | Self::Import
            | Self::Inherit
            | Self::External
            | Self::ObjectKey { .. } => None,
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
    /// The path argument to `inherit("path")`. The `String` is the owning
    /// `let` binding's name (e.g. `"base"` for `let base = inherit(...)`).
    InheritPath(String),
    /// The path argument to `import("path")`. The `String` is the owning
    /// `let` binding's name (e.g. `"helpers"` for `let helpers = import(...)`).
    ImportPath(String),
    /// A member accessed through an imported module (e.g. `fn_name` in `mod::fn_name(args)`).
    /// For nested access like `a::b::c`, path is `["a", "b"]` and member is `"c"`.
    ImportedMember {
        path: Vec<String>,
        member: String,
    },
    /// A builtin combinator keyword (e.g. `seq`, `choice`, `repeat`).
    Builtin,
}

impl RefKind {
    /// True if this reference carries an explicit `a::b`-style qualifier in
    /// source (`BaseRule` or `ImportedMember`). Bare-name references to a
    /// helper rule / external also resolve cross-module - via
    /// `Module::resolve_bare_name` - but their `RefKind` is the unqualified
    /// `Rule` / `Variable`, so they return `false` here.
    #[must_use]
    pub const fn is_qualified(&self) -> bool {
        matches!(self, Self::BaseRule(_) | Self::ImportedMember { .. })
    }
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
            RefKind::InheritPath(_) | RefKind::ImportPath(_) => false,
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

/// Where a bare-name lookup landed. Distinguishes "in this file" (so the
/// caller can use `module.rope` and the current URI) from "in some external
/// module" (where the caller needs the external module's path + rope).
#[derive(Clone, Copy)]
pub enum BindingLocation<'a> {
    Local(&'a Definition),
    External {
        module: &'a Module,
        def: &'a Definition,
    },
}

/// Walk an external module recursively for a top-level definition with
/// `name`, returning the owning module and the def. Used by
/// `Module::resolve_bare_name`.
#[must_use]
fn find_in_module<'a>(module: &'a Module, name: &str) -> Option<(&'a Module, &'a Definition)> {
    if let Some(defs) = module.definitions.as_ref()
        && let Some(def) = defs.iter().find(|d| d.name == name)
    {
        return Some((module, def));
    }
    for (_, sub) in &module.import_modules {
        if let Some(found) = find_in_module(sub, name) {
            return Some(found);
        }
    }
    None
}

/// A single analyzed grammar file. Used both for the root document the user
/// is editing and (recursively) for each inherited / imported file reachable
/// from it.
///
/// Each `Option<Vec<_>>` field tracks "did the pipeline reach this stage?"
/// distinctly from "did it produce an empty result?" - critical for
/// `cursor_context` and similar to be honest about not knowing.
///
/// Inferred types for `let` bindings are stored on `DefKind::Let.ty` rather
/// than via a separate borrow into `source`, since `TypeEnv` borrows the
/// source text and can't be co-stored with an owned `source: String`.
#[derive(Clone)]
pub struct Module {
    /// Canonical on-disk path. Always set: a `Module` represents an analyzed
    /// file. Non-file URIs short-circuit before construction.
    pub path: PathBuf,
    pub source: String,
    pub rope: Rope,
    /// Lexer tokens. Available after a successful lex.
    pub tokens: Option<Vec<Token>>,
    /// Definitions extracted from the AST. Available after parse.
    pub definitions: Option<Vec<Definition>>,
    /// References extracted from the resolved AST. Available after resolve.
    pub references: Option<Vec<Reference>>,
    /// Span of the grammar block (if present). Available after parse.
    /// Always `None` for helper / inherited modules (they have no grammar block).
    pub grammar_span: Option<Span>,
    /// Parsed info from the inherited base grammar (for go-to-def, references,
    /// and completion on `base::rule_name`). Available after stage 4. Always
    /// `None` for non-root modules in practice.
    pub base_module: Option<Box<Self>>,
    /// Imported modules, paired with their let-binding name (e.g. `"helpers"`
    /// for `let helpers = import("helpers.tsg")`). Recursive.
    pub import_modules: Vec<(String, Self)>,
    /// Whether the full Loader pipeline ran successfully (parse + validate +
    /// resolve + typecheck + lower + load all inherits/imports). When false on
    /// the root, `base_module` and `import_modules` reflect the manual-parse
    /// fallback and may be empty even if the file textually has imports.
    /// Externals are constructed only after their own loader succeeded, so
    /// this is always `true` for them.
    pub loader_succeeded: bool,
    /// Top-level declarations gated off by a disabled `#[cfg(...)]` attribute.
    /// Used to dim the disabled source range and surface a HINT diagnostic.
    /// NOTE: inline disabled items (e.g. `seq(a, #[cfg(x)] b, c)` where `x`
    /// is off) are NOT recorded here yet - they vanish from list nodes during
    /// `apply_cfg` and would need an upstream side table to recover.
    pub disabled_regions: Vec<DisabledRegion>,
    /// Cfg flags declared anywhere in this file's load chain, with their
    /// resolved enabled/disabled state. Used for hover and completion inside
    /// `#[cfg(...)]` attributes.
    pub declared_cfg_flags: Vec<CfgFlag>,
    /// AST arena + pools shared across this module and any externals it
    /// references. Retained on the analysis so showcase features
    /// (macro expansion preview, lowered grammar dump, ...) can re-walk
    /// the AST without re-running the lex/parse pipeline. `Arc`-shared so
    /// nested `base_module` / `import_modules` cheaply point at the same
    /// arena they were built from.
    pub shared: Arc<SharedAst>,
}

/// A top-level declaration disabled by `#[cfg(NAME)]`. `full_span` covers the
/// attribute + the gated declaration; `name_span` covers just `NAME` inside
/// the attribute.
#[derive(Clone, Debug)]
pub struct DisabledRegion {
    pub name: String,
    pub name_span: Span,
    pub full_span: Span,
}

/// A cfg flag declared in some module's `flags: { enabled: [...], disabled: [...] }`
/// block, with its resolved active state.
#[derive(Clone, Debug)]
pub struct CfgFlag {
    pub name: String,
    pub enabled: bool,
}

impl Module {
    /// Construct a shell `Module` with only the source/rope populated. Used
    /// when lex or parse fails before we can produce any analysis data. The
    /// `shared` arena is empty - no AST nodes were produced.
    #[must_use]
    pub fn empty(path: PathBuf, source: String, rope: Rope) -> Self {
        Self {
            path,
            source,
            rope,
            tokens: None,
            definitions: None,
            references: None,
            grammar_span: None,
            base_module: None,
            import_modules: Vec::new(),
            loader_succeeded: false,
            disabled_regions: Vec::new(),
            declared_cfg_flags: Vec::new(),
            shared: Arc::new(SharedAst::new(0)),
        }
    }

    /// Convert this module's `disabled_regions` into LSP HINT diagnostics
    /// tagged `UNNECESSARY`. Most editors render that as faded/dimmed text
    /// with the message available on hover - the rust-analyzer treatment for
    /// cfg-disabled code.
    #[must_use]
    pub fn cfg_hint_diagnostics(&self) -> Vec<tower_lsp::lsp_types::Diagnostic> {
        use tower_lsp::lsp_types::{DiagnosticSeverity, DiagnosticTag};
        self.disabled_regions
            .iter()
            .map(|r| tower_lsp::lsp_types::Diagnostic {
                range: crate::text::span_to_range(&self.rope, r.full_span),
                severity: Some(DiagnosticSeverity::HINT),
                source: Some("ts_grammar_ls".into()),
                message: format!("disabled by cfg flag `{}` (currently off)", r.name),
                tags: Some(vec![DiagnosticTag::UNNECESSARY]),
                ..Default::default()
            })
            .collect()
    }

    /// Look up a direct sub-import by binding name.
    #[must_use]
    pub fn get_submodule(&self, name: &str) -> Option<&Self> {
        self.import_modules
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, info)| info)
    }

    /// Iterate the spans of bare-name occurrences of `word` in this module:
    /// definition sites whose name matches, plus `Rule`/`Variable` references
    /// (the kinds that bind without an explicit `::` qualifier). Yields
    /// `(span, is_decl)` so callers can filter on the LSP `includeDeclaration`
    /// flag. Used by cross-file rename and find-references when walking the
    /// owning module of an inherited/imported binding.
    pub fn bare_name_occurrences<'a>(
        &'a self,
        word: &'a str,
    ) -> impl Iterator<Item = (Span, bool)> + 'a {
        let defs = self
            .definitions
            .iter()
            .flatten()
            .filter(move |d| d.name == word)
            .map(|d| (d.name_span, true));
        let refs = self
            .references
            .iter()
            .flatten()
            .filter(move |r| {
                matches!(&r.kind, RefKind::Rule(n) | RefKind::Variable(n) if n == word)
            })
            .map(|r| (r.span, false));
        defs.chain(refs)
    }

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
                | RefKind::ImportPath(_)
                | RefKind::InheritPath(_)
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

    /// Resolve a bare `name` from `scope` to either a local binding or an
    /// external module's definition. Local lookup takes precedence (lexical
    /// scoping); on miss we walk inherited base + transitive imports for a
    /// matching def, since helper rules and externals are reachable from the
    /// importer by bare name.
    #[must_use]
    pub fn resolve_bare_name(
        &self,
        name: &str,
        scope: Option<Span>,
    ) -> Option<BindingLocation<'_>> {
        if let Some(def) = self.binding_for(name, scope) {
            return Some(BindingLocation::Local(def));
        }
        if let Some(base) = self.base_module.as_deref()
            && let Some((module, def)) = find_in_module(base, name)
        {
            return Some(BindingLocation::External { module, def });
        }
        for (_, info) in &self.import_modules {
            if let Some((module, def)) = find_in_module(info, name) {
                return Some(BindingLocation::External { module, def });
            }
        }
        None
    }

    /// Walk a qualified-access chain to the leaf module. For `a::b::c`, given
    /// `path = ["a", "b"]`, returns the `Module` for `b` (a's sub-import).
    /// The first segment must be a top-level binding (import or inherit);
    /// subsequent segments walk through nested sub-imports.
    #[must_use]
    pub fn resolve_import_chain(&self, path: &[String]) -> Option<&Self> {
        let first = path.first()?;
        let mut module = self.get_module(first.as_str())?;
        for segment in &path[1..] {
            module = module.get_submodule(segment)?;
        }
        Some(module)
    }

    /// At `offset` on a qualified-member reference (`a::b::foo`), return the
    /// leaf module the qualifier resolves to. Walks the full chain via
    /// `resolve_import_chain`, so 3-level access like `h::utils::foo` lands
    /// on `utils`'s module, not on `h`'s.
    #[must_use]
    pub fn qualified_member_module(&self, offset: u32) -> Option<&Self> {
        match &self.reference_at(offset)?.kind {
            RefKind::ImportedMember { path, .. } => self.resolve_import_chain(path),
            RefKind::BaseRule(_) => self.base_module.as_deref(),
            _ => None,
        }
    }

    /// Look up a module by its variable name. Checks both imported modules
    /// and the inherited base grammar.
    #[must_use]
    pub fn get_module(&self, name: &str) -> Option<&Self> {
        if let Some((_, info)) = self.import_modules.iter().find(|(n, _)| n == name) {
            return Some(info);
        }
        if let Some(base) = self.base_module.as_deref()
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
                DefKind::Parameter { scope, .. } => scope,
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

    /// Classify what kind of identifier the cursor is on. Returns `None` when
    /// lex never produced tokens (we can't honestly answer).
    #[must_use]
    pub fn cursor_context(&self, offset: u32, source: &str) -> Option<CursorContext> {
        let tokens = self.tokens.as_deref()?;
        if let Some(gs) = self.grammar_span
            && crate::text::is_grammar_config_field(tokens, gs, offset)
        {
            return Some(CursorContext::GrammarConfigField);
        }
        // Use the resolved reference at the cursor (when available) to route
        // qualified access correctly even for multi-level chains like
        // `h::utils::foo`. The token walkback below can only see the
        // immediate predecessor, which misclassifies 3-level chains.
        if let Some(reference) = self.reference_at(offset) {
            match &reference.kind {
                RefKind::ImportedMember { .. } => {
                    return Some(CursorContext::ImportModuleAccess {
                        scope: self.scope_at(offset),
                    });
                }
                RefKind::BaseRule(_) => return Some(CursorContext::BaseRuleAccess),
                _ => {}
            }
        }
        // No resolved reference (yet) - fall back to a token-based check so
        // we can still classify `base::foo` / `h::foo` before the resolver
        // runs (e.g. mid-keystroke states where the reference list is empty).
        if crate::text::is_base_rule_access(tokens, offset) {
            if let Some(qualifier) = crate::text::qualified_access_module(tokens, source, offset)
                && self.definitions.as_ref().is_some_and(|defs| {
                    defs.iter()
                        .any(|d| d.name == qualifier && d.kind == DefKind::Import)
                })
            {
                return Some(CursorContext::ImportModuleAccess {
                    scope: self.scope_at(offset),
                });
            }
            return Some(CursorContext::BaseRuleAccess);
        }
        Some(CursorContext::Identifier {
            scope: self.scope_at(offset),
        })
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
    /// Diagnostics from the DSL pipeline (lex/parse/resolve/typecheck).
    /// Re-run and replaced on every `did_change`.
    pub dsl_diagnostics: Vec<Diagnostic>,
    /// Diagnostics from the full generate pipeline (subprocess). Preserved
    /// across `did_change` so they keep showing between saves; replaced when
    /// a new generate-check completes.
    pub generate_diagnostics: Vec<Diagnostic>,
    /// Last analysis where parse succeeded. Used as a fallback when the
    /// current text fails to parse, so handlers (hover, completion, ...) keep
    /// working mid-keystroke. Never consulted on the success path: every
    /// `get_analysis` re-runs analyze and serves the fresh result if it
    /// parsed.
    pub last_good_analysis: Option<std::sync::Arc<Module>>,
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
            ty: Ty::RULE,
        };
        assert!(!param.visible_from(None));
    }

    #[test]
    fn visible_from_same_scope() {
        let param = DefKind::Parameter {
            scope: Span::new(10, 50),
            ty: Ty::RULE,
        };
        assert!(param.visible_from(Some(Span::new(10, 50))));
    }

    #[test]
    fn visible_from_inner_scope() {
        // Param defined in outer scope [10, 100], cursor in inner scope [20, 50].
        let param = DefKind::Parameter {
            scope: Span::new(10, 100),
            ty: Ty::RULE,
        };
        assert!(param.visible_from(Some(Span::new(20, 50))));
    }

    #[test]
    fn visible_from_outer_scope_not_visible() {
        // Param defined in inner scope [20, 50], cursor in outer scope [10, 100].
        let param = DefKind::Parameter {
            scope: Span::new(20, 50),
            ty: Ty::RULE,
        };
        assert!(!param.visible_from(Some(Span::new(10, 100))));
    }

    #[test]
    fn visible_from_disjoint_scope() {
        let param = DefKind::Parameter {
            scope: Span::new(10, 50),
            ty: Ty::RULE,
        };
        assert!(!param.visible_from(Some(Span::new(60, 100))));
    }
}
