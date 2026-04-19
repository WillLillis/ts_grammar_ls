use std::sync::Arc;

use serde_json::to_value;
use tower::{Service, ServiceExt};
use tower_lsp::LspService;
use tower_lsp::jsonrpc::Request;
use tower_lsp::lsp_types::notification::{
    DidChangeConfiguration, DidCloseTextDocument, DidOpenTextDocument, DidSaveTextDocument,
};
use tower_lsp::lsp_types::request::{
    CodeActionRequest, Completion, DocumentHighlightRequest, DocumentSymbolRequest, Formatting,
    GotoDefinition, HoverRequest, Initialize, References, SemanticTokensFullRequest,
};
use tower_lsp::lsp_types::*;

use ts_grammar_ls::config::{Config, DiagnosticConfig};
use ts_grammar_ls::hover_docs;
use ts_grammar_ls::server::Backend;

const ID: i64 = 1;

async fn init(documents: &[(Url, &str)]) -> LspService<Backend> {
    // Disable generate diagnostics in tests - the test binary doesn't have
    // the generate-check subcommand.
    let config = Config {
        diagnostics: DiagnosticConfig {
            generate_diagnostics: false,
        },
        ..Default::default()
    };
    let (mut service, socket) = LspService::build(|client| Backend {
        client,
        document_map: Arc::new(dashmap::DashMap::new()),
        debounce_version: Arc::new(dashmap::DashMap::new()),
        generate_child: Arc::default(),
        config: Arc::new(config.into()),
        grammar_cache: Arc::new(dashmap::DashMap::new()),
    })
    .finish();

    // Drain server-to-client messages so internal buffers don't fill up
    // (multiple `publish_diagnostics` calls would otherwise deadlock).
    let mut socket = socket;
    tokio::spawn(async move {
        use futures_util::stream::StreamExt;
        while socket.next().await.is_some() {}
    });

    lsp_request::<Initialize>(
        &mut service,
        InitializeParams {
            capabilities: ClientCapabilities::default(),
            initialization_options: Some(serde_json::json!({
                "diagnostics": { "generate_diagnostics": false }
            })),
            ..Default::default()
        },
    )
    .await;

    for (uri, text) in documents {
        lsp_notify::<DidOpenTextDocument>(
            &mut service,
            DidOpenTextDocumentParams {
                text_document: TextDocumentItem {
                    uri: uri.clone(),
                    language_id: "tsg".into(),
                    version: 0,
                    text: text.to_string(),
                },
            },
        )
        .await;
    }

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    service
}

async fn lsp_request<R: tower_lsp::lsp_types::request::Request>(
    service: &mut LspService<Backend>,
    params: R::Params,
) -> R::Result {
    let resp = service
        .ready()
        .await
        .unwrap()
        .call(
            Request::build(R::METHOD)
                .id(ID)
                .params(to_value(params).unwrap())
                .finish(),
        )
        .await
        .unwrap()
        .unwrap();
    serde_json::from_value(resp.result().unwrap().clone()).unwrap()
}

async fn lsp_notify<R: tower_lsp::lsp_types::notification::Notification>(
    service: &mut LspService<Backend>,
    params: R::Params,
) {
    service
        .ready()
        .await
        .unwrap()
        .call(
            Request::build(R::METHOD)
                .params(to_value(params).unwrap())
                .finish(),
        )
        .await
        .unwrap();
}

fn test_uri() -> Url {
    Url::parse("file:///tmp/test_grammar.tsg").unwrap()
}

// ---------------------------------------------------------------------------
// Request helpers - cut boilerplate for common LSP requests
// ---------------------------------------------------------------------------

async fn hover_at(service: &mut LspService<Backend>, uri: Url, pos: Position) -> Option<Hover> {
    lsp_request::<HoverRequest>(
        service,
        HoverParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
}

async fn goto_def_at(
    service: &mut LspService<Backend>,
    uri: Url,
    pos: Position,
) -> Option<GotoDefinitionResponse> {
    lsp_request::<GotoDefinition>(
        service,
        GotoDefinitionParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
}

async fn references_at(
    service: &mut LspService<Backend>,
    uri: Url,
    pos: Position,
    include_declaration: bool,
) -> Option<Vec<Location>> {
    lsp_request::<References>(
        service,
        ReferenceParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: ReferenceContext {
                include_declaration,
            },
        },
    )
    .await
}

async fn highlights_at(
    service: &mut LspService<Backend>,
    uri: Url,
    pos: Position,
) -> Option<Vec<DocumentHighlight>> {
    lsp_request::<DocumentHighlightRequest>(
        service,
        DocumentHighlightParams {
            text_document_position_params: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
}

async fn completions_at(
    service: &mut LspService<Backend>,
    uri: Url,
    pos: Position,
) -> Vec<CompletionItem> {
    let result: Option<CompletionResponse> = lsp_request::<Completion>(
        service,
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri },
                position: pos,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: None,
        },
    )
    .await;
    match result {
        Some(CompletionResponse::Array(items)) => items,
        _ => Vec::new(),
    }
}

#[expect(clippy::unnecessary_wraps, reason = "match usage in assertions")]
fn make_hover(value: &str) -> Option<Hover> {
    Some(Hover {
        contents: HoverContents::Markup(MarkupContent {
            kind: MarkupKind::Markdown,
            value: value.to_string(),
        }),
        range: None,
    })
}

const SIMPLE_GRAMMAR: &str = r#"
grammar { language: "test" }

fn commaSep1(item: rule_t) -> rule_t {
    seq(item, repeat(seq(",", item)))
}

fn commaSep(item: rule_t) -> rule_t { optional(commaSep1(item)) }

let PREC = { ADD: 1, MUL: 2 }

rule program { repeat(_statement) }

rule _statement { choice(expression_statement, return_statement) }

rule expression_statement { seq(_expression, ";") }

rule return_statement { seq("return", optional(_expression), ";") }

rule _expression {
    choice(
        binary_expression,
        identifier,
        number,
    )
}

rule binary_expression {
    prec_left(PREC.ADD, seq(field(left, _expression), "+", field(right, _expression)))
}

rule identifier { regexp(r"[a-zA-Z_]\w*") }
rule number { regexp(r"[0-9]+") }
"#;

// ---------------------------------------------------------------------------
// Hover tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn hover_builtin_combinator() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Hover over "seq" in the commaSep1 function body (line 4, col 4)
    let result = hover_at(&mut service, test_uri(), Position::new(4, 4)).await;

    assert_eq!(result, make_hover(hover_docs::SEQ));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_user_defined_rule() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Hover over "program" reference in _statement's body won't work since
    // it's not referenced. Instead hover on the rule name itself.
    // "rule program" is on line 11
    let result = hover_at(&mut service, test_uri(), Position::new(11, 6)).await;

    assert_eq!(result, make_hover("```\nrule program\n```"));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_user_defined_function() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // "fn commaSep1" is on line 3, col 3
    let result = hover_at(&mut service, test_uri(), Position::new(3, 4)).await;

    assert_eq!(
        result,
        make_hover("```\nfn commaSep1(item: rule_t) -> rule_t\n```")
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_keyword() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Hover over "let" on line 9
    let result = hover_at(&mut service, test_uri(), Position::new(9, 0)).await;

    assert_eq!(result, make_hover(hover_docs::KW_LET));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_returns_none_on_whitespace() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let result = hover_at(&mut service, test_uri(), Position::new(0, 0)).await;

    assert!(result.is_none());
}

// ---------------------------------------------------------------------------
// Go-to-definition tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn goto_def_rule_reference() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // In _statement (line 13), "expression_statement" is a rule reference at col 24.
    let result = goto_def_at(&mut service, test_uri(), Position::new(13, 28)).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, test_uri());
    // Should point to the name span of `rule expression_statement` on line 15.
    assert_eq!(loc.range.start.line, 15);
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_function_reference() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // In commaSep body, "commaSep1" is a function call.
    // Line 7: fn commaSep(item: rule_t) -> rule_t { optional(commaSep1(item)) }
    // "commaSep1" starts at col 53
    let result = goto_def_at(&mut service, test_uri(), Position::new(7, 53)).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, test_uri());
    // Should point to fn commaSep1 definition on line 3
    assert_eq!(loc.range.start.line, 3);
}

// ---------------------------------------------------------------------------
// Completion tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn completion_includes_rules_and_builtins() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let result: Option<CompletionResponse> = lsp_request::<Completion>(
        &mut service,
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: test_uri() },
                position: Position::new(12, 0),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: None,
        },
    )
    .await;

    let CompletionResponse::Array(items) = result.unwrap() else {
        panic!("expected array response");
    };

    let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();

    // Should include user-defined rules
    assert!(labels.contains(&"program"));
    assert!(labels.contains(&"binary_expression"));
    // Should include user-defined functions
    assert!(labels.contains(&"commaSep1"));
    assert!(labels.contains(&"commaSep"));
    // Should include user-defined let bindings
    assert!(labels.contains(&"PREC"));
    // Should include builtin combinators
    assert!(labels.contains(&"seq"));
    assert!(labels.contains(&"choice"));
    assert!(labels.contains(&"token_immediate"));
    // Should include keywords
    assert!(labels.contains(&"rule"));
    assert!(labels.contains(&"fn"));
    // Should include type keywords
    assert!(labels.contains(&"rule_t"));
    assert!(labels.contains(&"str_t"));
}

// ---------------------------------------------------------------------------
// Document symbol tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_lists_all_definitions() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let DocumentSymbolResponse::Nested(symbols) = result.unwrap() else {
        panic!("expected nested symbols");
    };

    let names: Vec<&str> = symbols.iter().map(|s| s.name.as_str()).collect();

    assert!(names.contains(&"program"));
    assert!(names.contains(&"_statement"));
    assert!(names.contains(&"commaSep1"));
    assert!(names.contains(&"commaSep"));
    assert!(names.contains(&"PREC"));
    assert!(names.contains(&"binary_expression"));

    // Check kinds
    let fn_sym = symbols.iter().find(|s| s.name == "commaSep1").unwrap();
    assert_eq!(fn_sym.kind, SymbolKind::FUNCTION);

    let rule_sym = symbols.iter().find(|s| s.name == "program").unwrap();
    assert_eq!(rule_sym.kind, SymbolKind::CLASS);

    let let_sym = symbols.iter().find(|s| s.name == "PREC").unwrap();
    assert_eq!(let_sym.kind, SymbolKind::VARIABLE);
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_exclude_object_keys() {
    // Object literal keys are kept as internal definitions (for field-access
    // completion, goto-def) but are not surfaced as outline symbols: they are
    // value literal members, not schema definitions. Matches how mainstream
    // code LSPs (tsserver, rust-analyzer, gopls, pylsp) treat object literals.
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1, MUL: 2 }
rule program { "x" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    #[expect(
        deprecated,
        reason = "DocumentSymbol::deprecated is deprecated but required by the struct"
    )]
    let expected = DocumentSymbolResponse::Nested(vec![
        DocumentSymbol {
            name: "PREC".into(),
            kind: SymbolKind::VARIABLE,
            detail: None,
            range: Range::new(Position::new(2, 0), Position::new(2, 29)),
            selection_range: Range::new(Position::new(2, 4), Position::new(2, 8)),
            tags: None,
            deprecated: None,
            children: None,
        },
        DocumentSymbol {
            name: "program".into(),
            kind: SymbolKind::CLASS,
            detail: None,
            range: Range::new(Position::new(3, 0), Position::new(3, 20)),
            selection_range: Range::new(Position::new(3, 5), Position::new(3, 12)),
            tags: None,
            deprecated: None,
            children: None,
        },
    ]);

    assert_eq!(result, Some(expected));
}

// ---------------------------------------------------------------------------
// Diagnostics tests
// ---------------------------------------------------------------------------

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn diagnostics_clean_grammar_no_errors() {
    let service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(doc.diagnostics.dsl, vec![]);
}

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn diagnostics_syntax_error() {
    let broken = r#"grammar { language: "test" } rule program {"#;
    let service = init(&[(test_uri(), broken)]).await;

    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(
        doc.diagnostics.dsl,
        vec![Diagnostic {
            range: Range::new(Position::new(0, 43), Position::new(0, 43)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("ts_grammar_ls".into()),
            message: "expected expression".into(),
            ..Default::default()
        }]
    );
}

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn diagnostics_type_error() {
    let bad_types = r#"
        grammar { language: "test" }
        fn bad(x: rule_t) -> int_t { x }
        rule program { "x" }
    "#;
    let service = init(&[(test_uri(), bad_types)]).await;

    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(
        doc.diagnostics.dsl,
        vec![Diagnostic {
            range: Range::new(Position::new(2, 37), Position::new(2, 38)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("ts_grammar_ls".into()),
            message: "expected int_t, got rule_t".into(),
            ..Default::default()
        }]
    );
}

// ---------------------------------------------------------------------------
// Semantic token tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn semantic_tokens_classifies_identifiers() {
    let grammar = r#"
grammar { language: "test" }
fn helper(x: rule_t) -> rule_t { x }
rule program { helper(identifier) }
rule identifier { regexp(r"[a-z]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let result: Option<SemanticTokensResult> = lsp_request::<SemanticTokensFullRequest>(
        &mut service,
        SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
        panic!("expected full tokens");
    };

    // Should have semantic tokens for identifiers only (not keywords/strings/etc).
    assert!(!tokens.data.is_empty(), "expected some semantic tokens");

    // Token types from the legend:
    // 0 = function, 1 = variable, 2 = type, 3 = class
    let token_types: Vec<u32> = tokens.data.iter().map(|t| t.token_type).collect();

    // Should contain function tokens (helper def + helper call)
    assert!(token_types.contains(&0), "expected function tokens");
    // Should contain class tokens (rule names: program, identifier)
    assert!(token_types.contains(&3), "expected class tokens");
    // Should contain type tokens (rule_t annotations)
    assert!(token_types.contains(&2), "expected type tokens");
}

// ---------------------------------------------------------------------------
// Hover with type info
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn hover_let_binding_shows_type() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1, MUL: 2 }
rule program { prec(PREC.ADD, "x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Hover over "PREC" on line 2
    let result = hover_at(&mut service, test_uri(), Position::new(2, 5)).await;

    assert_eq!(result, make_hover("```\nlet PREC: object<int_t>\n```"));
}

// ---------------------------------------------------------------------------
// Go-to-definition: object fields
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn goto_def_object_field() {
    // PREC is on line 2, ADD is at col 16
    // Usage of PREC.ADD is on line 3
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1, MUL: 2 }
rule program { prec(PREC.ADD, "x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "ADD" in "PREC.ADD" on line 3
    // line 3: `rule program { prec(PREC.ADD, "x") }`
    //                                   ^ col 25
    let result = goto_def_at(&mut service, test_uri(), Position::new(3, 25)).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, test_uri());
    // Should point to "ADD" in the object literal on line 2
    assert_eq!(loc.range.start.line, 2);
}

// ---------------------------------------------------------------------------
// Go-to-definition: inherited rules and inherit path
// ---------------------------------------------------------------------------

/// Create a temporary directory with a base grammar and a derived grammar
/// that inherits from it.
struct InheritFixture {
    _dir: tempfile::TempDir,
    base_path: std::path::PathBuf,
    _base_text: String,
    derived_path: std::path::PathBuf,
    derived_text: String,
}

fn create_inherit_fixture() -> InheritFixture {
    let dir = tempfile::tempdir().unwrap();

    let base_text = r#"
grammar { language: "base_lang" }
rule program { repeat(_statement) }
rule _statement { choice(expression, "x") }
rule expression { choice(identifier, number) }
rule identifier { regexp(r"[a-z]+") }
rule number { regexp(r"[0-9]+") }
"#;
    let base_path = dir.path().join("base.tsg");
    std::fs::write(&base_path, base_text).unwrap();

    let derived_text = format!(
        r#"
let base = inherit("{}")
grammar {{
    language: "derived_lang",
    inherits: base,
    extras: base.extras,
}}
override rule _statement {{ choice(base::_statement, new_rule) }}
rule new_rule {{ seq("new", expression) }}
"#,
        base_path.display()
    );
    let derived_path = dir.path().join("derived.tsg");
    std::fs::write(&derived_path, &derived_text).unwrap();

    InheritFixture {
        _dir: dir,
        base_path,
        _base_text: base_text.to_string(),
        derived_path,
        derived_text,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_inherited_rule() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // "expression" on the last line: `rule new_rule { seq("new", expression) }`
    // This is an inherited rule - goto-def should jump to the base grammar.
    let expr_offset = fix.derived_text.find("expression").unwrap();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, expr_offset as u32);

    let result = goto_def_at(&mut service, derived_uri, pos).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, base_uri, "should jump to base grammar");
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_base_inline_rule() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // "base::_statement" - cursor on "_statement" after "::"
    let base_stmt_offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, base_stmt_offset as u32);

    let result = goto_def_at(&mut service, derived_uri, pos).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, base_uri, "base:: should jump to base grammar");
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_inherit_path() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor on the path string inside inherit("...")
    let path_offset = fix
        .derived_text
        .find(&fix.base_path.display().to_string())
        .unwrap();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, path_offset as u32);

    let result = goto_def_at(&mut service, derived_uri, pos).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, base_uri, "should jump to base grammar file");
}

// ---------------------------------------------------------------------------
// Additional hover coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn hover_type_keyword() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Hover over "rule_t" in fn signature (line 3: fn commaSep1(item: rule_t) ...)
    // "rule_t" starts at col 19
    let result = hover_at(&mut service, test_uri(), Position::new(3, 20)).await;

    assert_eq!(result, make_hover(hover_docs::TYPE_RULE_T));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_print_keyword() {
    // `print` is a new top-level debug-print keyword. Hovering it should
    // surface KW_PRINT docs instead of e.g. treating it as an identifier.
    let grammar = r#"
grammar { language: "test" }
print("hello")
rule program { "x" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // line 2: `print("hello")` - 'p' of print is at col 0
    let result = hover_at(&mut service, test_uri(), Position::new(2, 2)).await;

    assert_eq!(result, make_hover(hover_docs::KW_PRINT));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_list_list_rule_type_keyword() {
    // `list_list_rule_t` is one of the types added when conflicts/precedences
    // were de-special-cased. Hovering on it should surface the new docs.
    let grammar = r#"
grammar { language: "test" }
let groups: list_list_rule_t = [[a], [b]]
rule program { "x" }
rule a { "a" }
rule b { "b" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // line 2: `let groups: list_list_rule_t = [[a], [b]]`
    //                     ^ col 12
    let result = hover_at(&mut service, test_uri(), Position::new(2, 13)).await;

    assert_eq!(result, make_hover(hover_docs::TYPE_LIST_LIST_RULE_T));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_variable_reference() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1 }
rule program { prec(PREC.ADD, "x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Hover over "PREC" in usage on line 3 (not definition)
    // line 3: `rule program { prec(PREC.ADD, "x") }`
    //                              ^ col 20
    let result = hover_at(&mut service, test_uri(), Position::new(3, 21)).await;

    assert_eq!(result, make_hover("```\nlet PREC: object<int_t>\n```"));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_object_field_shows_value() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 10, MUL: 20 }
rule program { prec(PREC.ADD, "x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Hover over "ADD" in `PREC.ADD` on line 3
    // line 3: `rule program { prec(PREC.ADD, "x") }`
    //                                   ^ col 25
    let result = hover_at(&mut service, test_uri(), Position::new(3, 25)).await;

    assert_eq!(result, make_hover("```\nPREC.ADD = 10\n```"));
}

// ---------------------------------------------------------------------------
// Additional go-to-definition coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn goto_def_let_variable_reference() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1 }
rule program { prec(PREC.ADD, "x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "PREC" in usage (line 3, col 21)
    let result = goto_def_at(&mut service, test_uri(), Position::new(3, 21)).await;

    let resp = result.unwrap();
    let GotoDefinitionResponse::Scalar(loc) = resp else {
        panic!("expected scalar location");
    };
    assert_eq!(loc.uri, test_uri());
    // Should jump to "PREC" definition on line 2
    assert_eq!(loc.range.start.line, 2);
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_returns_none_for_builtin() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Cursor on "seq" builtin (line 4, col 4)
    let result = goto_def_at(&mut service, test_uri(), Position::new(4, 5)).await;

    assert!(result.is_none(), "builtins should not have a definition");
}

// ---------------------------------------------------------------------------
// Additional completion coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn completion_item_kinds_are_correct() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let result: Option<CompletionResponse> = lsp_request::<Completion>(
        &mut service,
        CompletionParams {
            text_document_position: TextDocumentPositionParams {
                text_document: TextDocumentIdentifier { uri: test_uri() },
                position: Position::new(12, 0),
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
            context: None,
        },
    )
    .await;

    let CompletionResponse::Array(items) = result.unwrap() else {
        panic!("expected array response");
    };

    let find = |name: &str| items.iter().find(|i| i.label == name).unwrap();

    assert_eq!(find("program").kind, Some(CompletionItemKind::CLASS));
    assert_eq!(find("commaSep1").kind, Some(CompletionItemKind::FUNCTION));
    assert_eq!(find("PREC").kind, Some(CompletionItemKind::VARIABLE));
    assert_eq!(find("seq").kind, Some(CompletionItemKind::FUNCTION));
    assert_eq!(find("rule").kind, Some(CompletionItemKind::KEYWORD));
    assert_eq!(
        find("rule_t").kind,
        Some(CompletionItemKind::TYPE_PARAMETER)
    );
}

#[tokio::test(flavor = "current_thread")]
async fn completion_object_fields_after_dot() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1, MUL: 2, SUB: 3 }
rule program { prec(PREC., "x") }"#;
    //                   ^ cursor after the dot
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor right after "PREC."
    let items = completions_at(&mut service, test_uri(), Position::new(3, 25)).await;

    let ci = |label: &str| CompletionItem {
        label: label.into(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(format!("PREC.{label}")),
        ..Default::default()
    };
    assert_eq!(items, vec![ci("ADD"), ci("MUL"), ci("SUB")]);
}

#[tokio::test(flavor = "current_thread")]
async fn completion_base_rules_after_double_colon() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Find "base::" and place cursor right after it.
    let offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let mut items = completions_at(&mut service, derived_uri, pos).await;
    items.sort_by(|a, b| a.label.cmp(&b.label));

    let ci = |label: &str| CompletionItem {
        label: label.into(),
        kind: Some(CompletionItemKind::CLASS),
        detail: Some(format!("rule {label} (base)")),
        ..Default::default()
    };
    assert_eq!(
        items,
        vec![
            ci("_statement"),
            ci("expression"),
            ci("identifier"),
            ci("number"),
            ci("program"),
        ]
    );
}

// ---------------------------------------------------------------------------
// Additional document symbol coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_have_valid_ranges() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let DocumentSymbolResponse::Nested(symbols) = result.unwrap() else {
        panic!("expected nested symbols");
    };

    for sym in &symbols {
        // selection_range should be within range
        assert!(
            sym.selection_range.start >= sym.range.start,
            "{}: selection start before range start",
            sym.name
        );
        assert!(
            sym.selection_range.end <= sym.range.end,
            "{}: selection end after range end",
            sym.name
        );
        // selection_range should be non-empty (name span)
        assert!(
            sym.selection_range.start != sym.selection_range.end
                || sym.selection_range.start.line != sym.selection_range.end.line,
            "{}: empty selection range",
            sym.name
        );
    }
}

// ---------------------------------------------------------------------------
// Additional diagnostics coverage
// ---------------------------------------------------------------------------

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn diagnostics_resolve_error() {
    let bad = r#"
        grammar { language: "test" }
        rule program { undefined_rule }
    "#;
    let service = init(&[(test_uri(), bad)]).await;

    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(
        doc.diagnostics.dsl,
        vec![Diagnostic {
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("ts_grammar_ls".into()),
            message: "unknown identifier 'undefined_rule'".into(),
            range: Range::new(Position::new(2, 23), Position::new(2, 37)),
            ..Default::default()
        }]
    );
}

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn diagnostics_lex_error() {
    let bad = r#"grammar { language: "test } rule program { "x" }"#;
    let service = init(&[(test_uri(), bad)]).await;

    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(
        doc.diagnostics.dsl,
        vec![Diagnostic {
            range: Range::new(Position::new(0, 45), Position::new(0, 48)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("ts_grammar_ls".into()),
            message: "unterminated string literal".into(),
            ..Default::default()
        }]
    );
}

// ---------------------------------------------------------------------------
// Additional semantic token coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn semantic_tokens_skips_non_ident_tokens() {
    let grammar = r#"
grammar { language: "test" }
rule program { seq("hello", repeat("world")) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let result: Option<SemanticTokensResult> = lsp_request::<SemanticTokensFullRequest>(
        &mut service,
        SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
        panic!("expected full tokens");
    };

    // This grammar has very few identifiers (just "program" at definition site).
    // Keywords (grammar, language, rule, seq, repeat) and strings ("hello", "world", "test")
    // should NOT produce semantic tokens.
    // The only semantic tokens should be for the "program" identifier.
    assert!(
        tokens.data.len() <= 2,
        "expected very few semantic tokens (only idents), got {}",
        tokens.data.len()
    );
}

// ---------------------------------------------------------------------------
// References tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn references_finds_all_usages() {
    // _expression is defined on line 5 and referenced on lines 3, 4, 7
    let grammar = r#"
grammar { language: "test" }
rule program { repeat(_expression) }
rule return_statement { seq("return", optional(_expression)) }

rule _expression { choice(identifier, number) }

rule identifier { regexp(r"[a-z]+") }
rule number { regexp(r"[0-9]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "_expression" definition (line 5, col 5)
    let result = references_at(&mut service, test_uri(), Position::new(5, 6), true).await;

    let mut locs = result.unwrap();
    locs.sort_by_key(|l| l.range.start.line);
    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    // Definition (line 5) + 2 references (lines 2, 3)
    assert_eq!(locs, vec![loc(2, 22, 33), loc(3, 47, 58), loc(5, 5, 16),]);
}

#[tokio::test(flavor = "current_thread")]
async fn references_finds_usages_inside_grammar_block() {
    // `identifier` is used in grammar fields (word, extras, inline, conflicts,
    // precedences) as well as in rule bodies.
    let grammar = r#"
grammar {
    language: "test",
    word: identifier,
    extras: [identifier, whitespace],
    inline: [identifier],
    conflicts: [[identifier, expression]],
    precedences: [[identifier, expression]],
}
rule program { seq(identifier, "x") }
rule expression { identifier }
rule identifier { regexp(r"[a-z]+") }
rule whitespace { regexp(r"\s+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on `identifier` in the definition (line 11).
    let result = references_at(&mut service, test_uri(), Position::new(11, 6), true).await;

    let mut locs = result.unwrap();
    locs.sort_by_key(|l| (l.range.start.line, l.range.start.character));

    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    assert_eq!(
        locs,
        vec![
            loc(3, 10, 20),  // word: identifier,
            loc(4, 13, 23),  // extras: [identifier, ...
            loc(5, 13, 23),  // inline: [identifier],
            loc(6, 17, 27),  // conflicts: [[identifier, ...
            loc(7, 19, 29),  // precedences: [[identifier, ...
            loc(9, 19, 29),  // rule program { seq(identifier, ...
            loc(10, 18, 28), // rule expression { identifier }
            loc(11, 5, 15),  // rule identifier { ... } (definition)
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_without_declaration() {
    let grammar = r#"
grammar { language: "test" }
rule program { repeat(_expression) }
rule return_statement { seq("return", optional(_expression)) }

rule _expression { choice(identifier, number) }

rule identifier { regexp(r"[a-z]+") }
rule number { regexp(r"[0-9]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let result = references_at(&mut service, test_uri(), Position::new(5, 6), false).await;

    let mut locs = result.unwrap();
    locs.sort_by_key(|l| l.range.start.line);
    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    assert_eq!(locs, vec![loc(2, 22, 33), loc(3, 47, 58),]);
}

#[tokio::test(flavor = "current_thread")]
async fn references_returns_none_for_unknown() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Cursor on whitespace
    let result = references_at(&mut service, test_uri(), Position::new(0, 0), true).await;

    assert_eq!(result, None);
}

// ---------------------------------------------------------------------------
// didChangeConfiguration tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn did_change_configuration_updates_config() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    // Default config has generate_diagnostics = true, but our test init
    // overrides it to false. Verify that.
    assert!(
        !service
            .inner()
            .config
            .read()
            .await
            .diagnostics
            .generate_diagnostics
    );

    // Send a config change enabling it.
    lsp_notify::<DidChangeConfiguration>(
        &mut service,
        DidChangeConfigurationParams {
            settings: serde_json::json!({
                "diagnostics": { "generate_diagnostics": true }
            }),
        },
    )
    .await;

    assert!(
        service
            .inner()
            .config
            .read()
            .await
            .diagnostics
            .generate_diagnostics
    );
}

#[tokio::test(flavor = "current_thread")]
async fn did_change_configuration_ignores_invalid() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let before = service
        .inner()
        .config
        .read()
        .await
        .diagnostics
        .generate_diagnostics;

    // Send garbage - should be silently ignored.
    lsp_notify::<DidChangeConfiguration>(
        &mut service,
        DidChangeConfigurationParams {
            settings: serde_json::json!("not an object"),
        },
    )
    .await;

    let after = service
        .inner()
        .config
        .read()
        .await
        .diagnostics
        .generate_diagnostics;

    assert_eq!(before, after);
}

// ---------------------------------------------------------------------------
// Additional hover coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn hover_grammar_config_field() {
    let grammar = r#"
grammar { language: "test" }
rule program { "x" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Hover on "language" inside the grammar block should show config field docs.
    let result = hover_at(&mut service, test_uri(), Position::new(1, 11)).await;

    assert_eq!(result, make_hover(hover_docs::CFG_LANGUAGE));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_reserved_config_field_not_builtin() {
    let grammar = r#"
grammar {
    language: "test",
    reserved: { default: ["if", "else"] },
}
rule program { "x" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Hover on "reserved" inside the grammar block should show config field
    // docs, NOT the reserved() builtin combinator docs.
    let result = hover_at(&mut service, test_uri(), Position::new(3, 6)).await;

    assert_eq!(result, make_hover(hover_docs::CFG_RESERVED));
}

#[tokio::test(flavor = "current_thread")]
async fn references_reserved_builtin_excludes_config_field() {
    let grammar = r#"
grammar {
    language: "test",
    reserved: { default: ["if"] },
}
rule program { reserved("default", identifier) }
rule identifier { regexp(r"[a-z]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // References on "reserved" in the rule body (builtin usage) should NOT
    // include the "reserved:" config field in the grammar block.
    let result = references_at(&mut service, test_uri(), Position::new(5, 17), false).await;

    let locs = result.expect("should find references");
    // Should only have the one usage in the rule body, not the config field.
    assert_eq!(
        locs,
        vec![Location {
            uri: test_uri(),
            range: Range::new(Position::new(5, 15), Position::new(5, 23)),
        }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_on_config_field_returns_none() {
    let grammar = r#"
grammar {
    language: "test",
    reserved: { default: ["if"] },
}
rule program { reserved("default", identifier) }
rule identifier { regexp(r"[a-z]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // References on "reserved:" config field should return None,
    // not show builtin reserved() usages.
    let result = references_at(&mut service, test_uri(), Position::new(3, 6), true).await;

    assert_eq!(result, None);
}

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_on_config_field_returns_none() {
    let grammar = r#"
grammar {
    language: "test",
    reserved: { default: ["if"] },
}
rule program { reserved("default", identifier) }
rule identifier { regexp(r"[a-z]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Highlight on "reserved:" config field should return None.
    let result = highlights_at(&mut service, test_uri(), Position::new(3, 6)).await;

    assert_eq!(result, None);
}

#[tokio::test(flavor = "current_thread")]
async fn hover_override_rule() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Find "override rule _statement" and hover on "_statement"
    let name_offset = fix.derived_text.find("_statement").unwrap();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, name_offset as u32);

    let result = hover_at(&mut service, derived_uri, pos).await;

    assert_eq!(result, make_hover("```\noverride rule _statement\n```"));
}

#[tokio::test(flavor = "current_thread")]
async fn hover_base_rule_shows_base_definition() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Hover on "_statement" in "base::_statement" should show the base
    // grammar's rule, not the local override.
    let base_ref_offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, base_ref_offset as u32);

    let result = hover_at(&mut service, derived_uri, pos).await;

    assert_eq!(result, make_hover("```\nrule _statement\n```"));
}

#[tokio::test(flavor = "current_thread")]
async fn references_base_rule_excludes_override() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // References on "_statement" in "base::_statement" should show:
    // 1. The base grammar's definition of _statement
    // 2. Usages of _statement in the base grammar (e.g. in rule program)
    // 3. The base::_statement reference in the derived grammar
    // But NOT the override rule _statement definition.
    let base_ref_offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, base_ref_offset as u32);

    let result = references_at(&mut service, derived_uri.clone(), pos, true).await;

    let locations = result.expect("should find references");

    // Should have locations in the base file (definition + usage in program)
    // and the derived file (the base::_statement reference).
    assert_eq!(
        locations,
        vec![
            Location {
                uri: base_uri.clone(),
                range: Range {
                    start: Position {
                        line: 3,
                        character: 5,
                    },
                    end: Position {
                        line: 3,
                        character: 15,
                    }
                }
            },
            Location {
                uri: base_uri,
                range: Range {
                    start: Position {
                        line: 2,
                        character: 22,
                    },
                    end: Position {
                        line: 2,
                        character: 32,
                    }
                }
            },
            Location {
                uri: derived_uri,
                range: Range {
                    start: Position {
                        line: 7,
                        character: 40,
                    },
                    end: Position {
                        line: 7,
                        character: 50,
                    }
                }
            }
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_base_rule_excludes_declaration_when_disabled() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();
    let base_uri = Url::from_file_path(&fix.base_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // With `include_declaration: false`, references on `base::_statement`
    // should NOT include the base grammar's definition - only usages.
    let base_ref_offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, base_ref_offset as u32);

    let result = references_at(&mut service, derived_uri.clone(), pos, false).await;

    assert_eq!(
        result.expect("should find references"),
        vec![
            // Usage of _statement in base's `rule program`.
            Location {
                uri: base_uri,
                range: Range::new(Position::new(2, 22), Position::new(2, 32)),
            },
            // The base::_statement reference in derived.
            Location {
                uri: derived_uri,
                range: Range::new(Position::new(7, 40), Position::new(7, 50)),
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_base_rule_excludes_override() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Highlight on "_statement" in "base::_statement" should only highlight
    // other base::_statement references, not the override rule definition.
    let base_ref_offset = fix.derived_text.find("base::_statement").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, base_ref_offset as u32);

    let result = highlights_at(&mut service, derived_uri, pos).await;

    let highlights = result.expect("should find highlights");

    // Should only contain the base::_statement reference (READ), not
    // the override rule definition (which would be WRITE).
    assert_eq!(
        highlights.len(),
        1,
        "should highlight only the base::_statement reference"
    );
    assert_eq!(highlights[0].kind, Some(DocumentHighlightKind::READ));
}

#[tokio::test(flavor = "current_thread")]
async fn base_grammar_uses_in_memory_text_when_open() {
    // When the base grammar is also open as a document, analyze() should use
    // its in-memory text rather than re-reading from disk. This is exercised
    // by opening the base with content that differs from what's on disk - the
    // derived grammar's references should reflect the in-memory version.
    let dir = tempfile::tempdir().unwrap();

    // Write base to disk WITHOUT `special_rule`.
    let base_on_disk = r#"
grammar { language: "base_lang" }
rule program { "x" }
"#;
    let base_path = dir.path().join("base.tsg");
    std::fs::write(&base_path, base_on_disk).unwrap();

    // Open base IN-MEMORY with `special_rule` added.
    let base_in_memory = r#"
grammar { language: "base_lang" }
rule program { "x" }
rule special_rule { "y" }
"#;

    let derived_text = format!(
        r#"
let base = inherit("{}")
grammar {{ language: "derived", inherits: base }}
rule use_it {{ base::special_rule }}
"#,
        base_path.display()
    );
    let derived_path = dir.path().join("derived.tsg");
    std::fs::write(&derived_path, &derived_text).unwrap();

    let base_uri = Url::from_file_path(&base_path).unwrap();
    let derived_uri = Url::from_file_path(&derived_path).unwrap();

    let mut service = init(&[
        (base_uri.clone(), base_in_memory),
        (derived_uri.clone(), &derived_text),
    ])
    .await;

    // Go to definition on `special_rule` in `base::special_rule`. This rule
    // only exists in the in-memory base grammar, not on disk.
    let ref_offset = derived_text.find("base::special_rule").unwrap() + "base::".len();
    let rope = ropey::Rope::from_str(&derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, ref_offset as u32);

    let result = goto_def_at(&mut service, derived_uri, pos).await;

    // If the cache used stale disk content, this would return None because
    // `special_rule` doesn't exist in the on-disk base.
    // `rule special_rule` is on line 3 of the in-memory base; name starts at col 5.
    assert_eq!(
        result,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: base_uri,
            range: Range::new(Position::new(3, 5), Position::new(3, 17)),
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_override_rule_excludes_base() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // References on "_statement" at the override rule definition should NOT
    // include locations from the base grammar or the base::_statement reference.
    let override_offset =
        fix.derived_text.find("override rule _statement").unwrap() + "override rule ".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, override_offset as u32);

    let result = references_at(&mut service, derived_uri.clone(), pos, true).await;

    let locations = result.expect("should find references");
    assert_eq!(
        locations,
        vec![Location {
            uri: derived_uri,
            range: Range {
                start: Position {
                    line: 7,
                    character: 14
                },
                end: Position {
                    line: 7,
                    character: 24
                }
            }
        }]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_override_rule_excludes_base_ref() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Highlight on "_statement" at the override rule definition should NOT
    // highlight the base::_statement reference.
    let override_offset =
        fix.derived_text.find("override rule _statement").unwrap() + "override rule ".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, override_offset as u32);

    let result = highlights_at(&mut service, derived_uri, pos).await;

    let highlights = result.expect("should find highlights");
    // Should only have the override definition (WRITE), not the base::_statement ref.
    //left: [DocumentHighlight { range: Range { start: Position { line: 7, character: 14 }, end: Position { line: 7, character: 24 } }, kind: Some(Write) }
    assert_eq!(
        highlights,
        vec![DocumentHighlight {
            range: Range {
                start: Position {
                    line: 7,
                    character: 14,
                },
                end: Position {
                    line: 7,
                    character: 24,
                }
            },
            kind: Some(DocumentHighlightKind::WRITE)
        }]
    );
}

// ---------------------------------------------------------------------------
// Additional go-to-definition coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn goto_def_parameter_name() {
    let grammar = r#"
grammar { language: "test" }
fn helper(item: rule_t) -> rule_t { seq(item, item) }
rule program { helper("x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // "item" usage inside fn body - line 2: `seq(item, item)`
    // First "item" in seq starts at col 40
    // Cursor on first "item" inside fn body
    let result = goto_def_at(&mut service, test_uri(), Position::new(2, 41)).await;

    // Should go to the parameter definition in the fn signature.
    assert_eq!(
        result,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: test_uri(),
            range: Range::new(Position::new(2, 10), Position::new(2, 14)),
        }))
    );
}

// ---------------------------------------------------------------------------
// Additional references coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn references_function_name() {
    let grammar = r#"
grammar { language: "test" }
fn helper(x: rule_t) -> rule_t { x }
rule program { helper(helper("x")) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "helper" definition (line 2, col 4)
    let result = references_at(&mut service, test_uri(), Position::new(2, 4), true).await;

    let mut locs = result.unwrap();
    locs.sort_by(|a, b| {
        a.range
            .start
            .line
            .cmp(&b.range.start.line)
            .then(a.range.start.character.cmp(&b.range.start.character))
    });
    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    // Definition (line 2) + 2 call sites (line 3)
    assert_eq!(locs, vec![loc(2, 3, 9), loc(3, 15, 21), loc(3, 22, 28),]);
}

// ---------------------------------------------------------------------------
// Additional semantic tokens coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn semantic_tokens_variable_and_class_refs() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1 }
rule _expression { choice(identifier, number) }
rule identifier { regexp(r"[a-z]+") }
rule number { regexp(r"[0-9]+") }
rule program { prec(PREC.ADD, _expression) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let result: Option<SemanticTokensResult> = lsp_request::<SemanticTokensFullRequest>(
        &mut service,
        SemanticTokensParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let SemanticTokensResult::Tokens(tokens) = result.unwrap() else {
        panic!("expected full tokens");
    };

    // Token types: 0=function, 1=variable, 2=type, 3=class
    let token_types: Vec<u32> = tokens.data.iter().map(|t| t.token_type).collect();

    // Should have variable tokens (PREC def + PREC ref)
    assert!(token_types.contains(&1), "expected variable tokens");
    // Should have class tokens (rule name defs + rule refs like _expression, identifier, number)
    assert!(token_types.contains(&3), "expected class tokens");
}

// ---------------------------------------------------------------------------
// Additional diagnostics coverage
// ---------------------------------------------------------------------------

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn diagnostics_lower_error() {
    // Calling a rule as a function is caught at typecheck.
    let bad = r#"
        grammar { language: "test" }
        rule foo { "x" }
        rule program { foo("y") }
    "#;
    let service = init(&[(test_uri(), bad)]).await;

    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(
        doc.diagnostics.dsl,
        vec![Diagnostic {
            range: Range::new(Position::new(3, 23), Position::new(3, 31)),
            severity: Some(DiagnosticSeverity::ERROR),
            source: Some("ts_grammar_ls".into()),
            message: "undefined function 'foo'".into(),
            ..Default::default()
        }]
    );
}

// ---------------------------------------------------------------------------
// Additional document symbol coverage
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_override_rule() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: derived_uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let DocumentSymbolResponse::Nested(symbols) = result.unwrap() else {
        panic!("expected nested symbols");
    };

    let names: Vec<(&str, SymbolKind)> =
        symbols.iter().map(|s| (s.name.as_str(), s.kind)).collect();
    assert!(names.contains(&("base", SymbolKind::VARIABLE)));
    assert!(names.contains(&("_statement", SymbolKind::CLASS)));
    assert!(names.contains(&("new_rule", SymbolKind::CLASS)));
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbols_function_has_detail() {
    let mut service = init(&[(test_uri(), SIMPLE_GRAMMAR)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let DocumentSymbolResponse::Nested(symbols) = result.unwrap() else {
        panic!("expected nested symbols");
    };

    let fn_sym = symbols.iter().find(|s| s.name == "commaSep1").unwrap();
    assert_eq!(
        fn_sym.detail,
        Some("fn commaSep1(item: rule_t) -> rule_t".into())
    );
}

// ---------------------------------------------------------------------------
// Document highlight tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_rule() {
    // _expression defined on line 3, referenced on lines 2 and 4
    let grammar = r#"
grammar { language: "test" }
rule program { _expression }
rule _expression { choice(identifier, number) }
rule identifier { prec(1, _expression) }
rule number { regexp(r"[0-9]+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "_expression" definition (line 3, col 5)
    let result = highlights_at(&mut service, test_uri(), Position::new(3, 6)).await;

    let mut highlights = result.unwrap();
    highlights.sort_by_key(|h| (h.range.start.line, h.range.start.character));

    assert_eq!(
        highlights,
        vec![
            DocumentHighlight {
                range: Range::new(Position::new(2, 15), Position::new(2, 26)),
                kind: Some(DocumentHighlightKind::READ),
            },
            DocumentHighlight {
                range: Range::new(Position::new(3, 5), Position::new(3, 16)),
                kind: Some(DocumentHighlightKind::WRITE),
            },
            DocumentHighlight {
                range: Range::new(Position::new(4, 26), Position::new(4, 37)),
                kind: Some(DocumentHighlightKind::READ),
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_rule_inside_grammar_block() {
    // `identifier` is used in grammar fields (word, extras, inline, conflicts,
    // precedences) as well as in rule bodies.
    let grammar = r#"
grammar {
    language: "test",
    word: identifier,
    extras: [identifier, whitespace],
    inline: [identifier],
    conflicts: [[identifier, expression]],
    precedences: [[identifier, expression]],
}
rule program { seq(identifier, "x") }
rule expression { identifier }
rule identifier { regexp(r"[a-z]+") }
rule whitespace { regexp(r"\s+") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on `identifier` in the definition (line 11).
    let result = highlights_at(&mut service, test_uri(), Position::new(11, 6)).await;

    let mut highlights = result.unwrap();
    highlights.sort_by_key(|h| (h.range.start.line, h.range.start.character));

    let read = |line, start, end| DocumentHighlight {
        range: Range::new(Position::new(line, start), Position::new(line, end)),
        kind: Some(DocumentHighlightKind::READ),
    };
    let write = |line, start, end| DocumentHighlight {
        range: Range::new(Position::new(line, start), Position::new(line, end)),
        kind: Some(DocumentHighlightKind::WRITE),
    };

    assert_eq!(
        highlights,
        vec![
            read(3, 10, 20),  // word: identifier,
            read(4, 13, 23),  // extras: [identifier, ...
            read(5, 13, 23),  // inline: [identifier],
            read(6, 17, 27),  // conflicts: [[identifier, ...
            read(7, 19, 29),  // precedences: [[identifier, ...
            read(9, 19, 29),  // rule program { seq(identifier, ...
            read(10, 18, 28), // rule expression { identifier }
            write(11, 5, 15), // rule identifier { ... } (definition)
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_object_field() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1, MUL: 2 }
rule a { prec(PREC.ADD, "x") }
rule b { prec(PREC.ADD, "y") }
rule program { choice(a, b) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "ADD" in PREC.ADD on line 3
    let result = highlights_at(&mut service, test_uri(), Position::new(3, 19)).await;

    let mut highlights = result.unwrap();
    highlights.sort_by_key(|h| (h.range.start.line, h.range.start.character));

    // Should highlight definition (line 2) and both uses (lines 3 and 4)
    assert_eq!(
        highlights,
        vec![
            DocumentHighlight {
                range: Range::new(Position::new(2, 13), Position::new(2, 16)),
                kind: Some(DocumentHighlightKind::WRITE),
            },
            DocumentHighlight {
                range: Range::new(Position::new(3, 19), Position::new(3, 22)),
                kind: Some(DocumentHighlightKind::READ),
            },
            DocumentHighlight {
                range: Range::new(Position::new(4, 19), Position::new(4, 22)),
                kind: Some(DocumentHighlightKind::READ),
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_highlight_builtin() {
    let grammar = r#"
grammar { language: "test" }
rule program { seq("a", seq("b", "c")) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on first "seq" (line 2, col 15)
    let result = highlights_at(&mut service, test_uri(), Position::new(2, 15)).await;

    let highlights = result.unwrap();
    // Should highlight both uses of seq on line 2
    assert_eq!(highlights.len(), 2);
    assert!(
        highlights
            .iter()
            .all(|h| h.kind == Some(DocumentHighlightKind::READ))
    );
}

// ---------------------------------------------------------------------------
// Builtin references tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn references_builtin() {
    let grammar = r#"
grammar { language: "test" }
rule a { repeat("x") }
rule b { repeat(repeat("y")) }
rule program { choice(a, b) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "repeat" (line 2, col 9)
    let result = references_at(&mut service, test_uri(), Position::new(2, 10), false).await;

    let mut locs = result.unwrap();
    locs.sort_by(|a, b| {
        a.range
            .start
            .line
            .cmp(&b.range.start.line)
            .then(a.range.start.character.cmp(&b.range.start.character))
    });

    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    // 3 uses of repeat: line 2 (1), line 3 (2)
    assert_eq!(locs, vec![loc(2, 9, 15), loc(3, 9, 15), loc(3, 16, 22),]);
}

// ---------------------------------------------------------------------------
// Object field references test
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn references_object_field() {
    let grammar = r#"
grammar { language: "test" }
let PREC = { ADD: 1, MUL: 2 }
rule a { prec(PREC.ADD, "x") }
rule b { prec(PREC.ADD, "y") }
rule program { choice(a, b) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "ADD" in PREC.ADD on line 3
    let result = references_at(&mut service, test_uri(), Position::new(3, 19), false).await;

    let mut locs = result.unwrap();
    locs.sort_by_key(|l| (l.range.start.line, l.range.start.character));

    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    assert_eq!(locs, vec![loc(3, 19, 22), loc(4, 19, 22),]);
}

// ---------------------------------------------------------------------------
// Parameter scoping tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn references_parameter_scoped() {
    // Two functions with same parameter name "item" - references should
    // only return matches within the same function.
    let grammar = r#"
grammar { language: "test" }
fn f1(item: rule_t) -> rule_t { seq(item, item) }
fn f2(item: rule_t) -> rule_t { repeat(item) }
rule program { f1(f2("x")) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on first "item" usage inside f1 body (line 2, col 36)
    let result = references_at(&mut service, test_uri(), Position::new(2, 36), true).await;

    let mut locs = result.unwrap();
    locs.sort_by(|a, b| a.range.start.character.cmp(&b.range.start.character));

    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    // Should only find references within f1: param def + 2 usages, NOT f2's "item".
    assert_eq!(locs, vec![loc(2, 6, 10), loc(2, 36, 40), loc(2, 42, 46)]);
}

#[tokio::test(flavor = "current_thread")]
async fn highlight_parameter_scoped() {
    let grammar = r#"
grammar { language: "test" }
fn f1(item: rule_t) -> rule_t { seq(item, item) }
fn f2(item: rule_t) -> rule_t { repeat(item) }
rule program { f1(f2("x")) }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on first "item" usage inside f1 body (line 2, col 36)
    let result = highlights_at(&mut service, test_uri(), Position::new(2, 36)).await;

    let mut highlights = result.unwrap();
    highlights.sort_by_key(|h| h.range.start.character);
    // Should only highlight within f1, not f2.
    assert_eq!(
        highlights,
        vec![
            DocumentHighlight {
                range: Range::new(Position::new(2, 6), Position::new(2, 10)),
                kind: Some(DocumentHighlightKind::WRITE),
            },
            DocumentHighlight {
                range: Range::new(Position::new(2, 36), Position::new(2, 40)),
                kind: Some(DocumentHighlightKind::READ),
            },
            DocumentHighlight {
                range: Range::new(Position::new(2, 42), Position::new(2, 46)),
                kind: Some(DocumentHighlightKind::READ),
            },
        ]
    );
}

// ---------------------------------------------------------------------------
// For-loop binding tests
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn goto_def_for_loop_binding() {
    let grammar = r#"
grammar { language: "test" }
let ops: list_rule_t = [("&&", 2), ("||", 1)]
rule program {
    choice(for (op: str_t, p: int_t) in ops {
        prec_left(p, seq("x", op, "x"))
    })
}
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "op" usage in seq on line 5, col 30
    let result = goto_def_at(&mut service, test_uri(), Position::new(5, 30)).await;

    // Should point to the "op" binding in the for-loop on line 4, col 16.
    assert_eq!(
        result,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: test_uri(),
            range: Range::new(Position::new(4, 16), Position::new(4, 18)),
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_for_loop_binding() {
    let grammar = r#"
grammar { language: "test" }
let ops: list_rule_t = [("&&", 2), ("||", 1)]
rule program {
    choice(for (op: str_t, p: int_t) in ops {
        prec_left(p, seq("x", op, "x"))
    })
}
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "p" usage in prec_left (line 5)
    let result = references_at(&mut service, test_uri(), Position::new(5, 18), true).await;

    let mut locs = result.unwrap();
    locs.sort_by_key(|l| (l.range.start.line, l.range.start.character));
    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    // Binding def (line 4, "p" at col 27) + usage (line 5, "p" at col 18)
    assert_eq!(locs, vec![loc(4, 27, 28), loc(5, 18, 19)]);
}

#[tokio::test(flavor = "current_thread")]
async fn references_parameter_nested_usage() {
    // Regression: item used inside nested seq/repeat should still be found.
    let grammar = r#"
grammar { language: "test" }
fn comma_sep1(item: rule_t) -> rule_t { seq(item, repeat(seq(",", item))) }
rule program { comma_sep1("x") }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Cursor on "item" parameter name in signature (line 2, col 14)
    let result = references_at(&mut service, test_uri(), Position::new(2, 14), true).await;

    let mut locs = result.unwrap();
    locs.sort_by_key(|l| l.range.start.character);
    let loc = |line, start, end| Location {
        uri: test_uri(),
        range: Range::new(Position::new(line, start), Position::new(line, end)),
    };
    // Parameter def (col 14) + 2 usages (col 44, col 66)
    assert_eq!(locs, vec![loc(2, 14, 18), loc(2, 44, 48), loc(2, 66, 70)]);
}

// ---------------------------------------------------------------------------
// Lifecycle handlers (did_save, did_close)
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn did_close_removes_document_from_map() {
    let grammar = "grammar { language: \"test\" }\nrule program { \"x\" }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    // Before close: hover finds `program` as a rule.
    let before = hover_at(&mut service, test_uri(), Position::new(1, 6)).await;
    assert_eq!(before, make_hover("```\nrule program\n```"));

    lsp_notify::<DidCloseTextDocument>(
        &mut service,
        DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
        },
    )
    .await;

    // After close: hover returns None because the doc is gone from the map.
    let after = hover_at(&mut service, test_uri(), Position::new(1, 6)).await;
    assert_eq!(after, None);
}

#[expect(clippy::significant_drop_tightening)]
#[tokio::test(flavor = "current_thread")]
async fn did_save_refreshes_cached_diagnostics() {
    // did_save re-runs the diagnostic pipeline. We can observe this by
    // stuffing the document's cached diagnostics with a sentinel, then
    // triggering did_save and checking that the pipeline overwrote them.
    let grammar = "grammar { language: \"test\" }\nrule program { \"x\" }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    let sentinel = Diagnostic {
        range: Range::new(Position::new(99, 0), Position::new(99, 0)),
        message: "stale".into(),
        ..Default::default()
    };
    service
        .inner()
        .document_map
        .get_mut(&test_uri())
        .unwrap()
        .diagnostics
        .dsl = vec![sentinel.clone()];

    lsp_notify::<DidSaveTextDocument>(
        &mut service,
        DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: test_uri() },
            text: None,
        },
    )
    .await;

    // Pipeline ran: cached diagnostics reflect the clean grammar, not the sentinel.
    let doc = service.inner().document_map.get(&test_uri()).unwrap();
    assert_eq!(doc.diagnostics.dsl, vec![]);
}

#[tokio::test(flavor = "current_thread")]
async fn did_save_on_unknown_uri_is_noop() {
    // Saving a URI that was never opened should early-return without panicking.
    let mut service = init(&[]).await;

    let unknown = Url::parse("file:///tmp/never-opened.tsg").unwrap();
    lsp_notify::<DidSaveTextDocument>(
        &mut service,
        DidSaveTextDocumentParams {
            text_document: TextDocumentIdentifier {
                uri: unknown.clone(),
            },
            text: None,
        },
    )
    .await;

    // Subsequent hover on the unknown URI still returns None (no doc).
    let result = hover_at(&mut service, unknown, Position::new(0, 0)).await;
    assert_eq!(result, None);
}

#[tokio::test(flavor = "current_thread")]
async fn did_close_on_unknown_uri_is_noop() {
    let mut service = init(&[]).await;

    let unknown = Url::parse("file:///tmp/never-opened.tsg").unwrap();
    lsp_notify::<DidCloseTextDocument>(
        &mut service,
        DidCloseTextDocumentParams {
            text_document: TextDocumentIdentifier { uri: unknown },
        },
    )
    .await;
    // If this returns without panic, the test passes.
}

// ---------------------------------------------------------------------------
// Formatting tests
// ---------------------------------------------------------------------------

async fn format_request(service: &mut LspService<Backend>, uri: Url) -> Option<Vec<TextEdit>> {
    lsp_request::<Formatting>(
        service,
        DocumentFormattingParams {
            text_document: TextDocumentIdentifier { uri },
            options: FormattingOptions {
                tab_size: 4,
                insert_spaces: true,
                ..Default::default()
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
        },
    )
    .await
}

#[tokio::test(flavor = "current_thread")]
async fn formatting_reformats_messy_source() {
    // Messy input with no padding around braces/colons; expect a single edit
    // replacing the whole document with the formatter's canonical output.
    let messy = "grammar{language:\"test\"}\n\nrule program{\"x\"}\n";
    let mut service = init(&[(test_uri(), messy)]).await;

    let result = format_request(&mut service, test_uri()).await;

    let expected_text = "grammar {\n    language: \"test\",\n}\n\nrule program { \"x\" }\n";
    assert_eq!(
        result,
        Some(vec![TextEdit {
            range: Range::new(Position::new(0, 0), Position::new(3, 0)),
            new_text: expected_text.into(),
        }])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn formatting_already_clean_returns_empty_edits() {
    // If the document is already formatted, return an empty edit list rather
    // than a no-op replacement. Saves the client from applying a redundant diff.
    let clean = "grammar {\n    language: \"test\",\n}\n\nrule program { \"x\" }\n";
    let mut service = init(&[(test_uri(), clean)]).await;

    let result = format_request(&mut service, test_uri()).await;

    assert_eq!(result, Some(Vec::new()));
}

#[tokio::test(flavor = "current_thread")]
async fn formatting_unparseable_returns_none() {
    // Incomplete source - parser fails, formatter returns None, handler
    // returns None. The client will leave the buffer unchanged.
    let broken = "grammar { language: \"test\" } rule program {";
    let mut service = init(&[(test_uri(), broken)]).await;

    let result = format_request(&mut service, test_uri()).await;

    assert_eq!(result, None);
}

#[tokio::test(flavor = "current_thread")]
async fn formatting_unknown_uri_returns_none() {
    // No document registered for this URI - handler must not panic.
    let mut service = init(&[]).await;

    let result = format_request(
        &mut service,
        Url::parse("file:///tmp/never-opened.tsg").unwrap(),
    )
    .await;

    assert_eq!(result, None);
}

// ---------------------------------------------------------------------------
// Code action tests
// ---------------------------------------------------------------------------

async fn code_actions_at(
    service: &mut LspService<Backend>,
    uri: Url,
    range: Range,
) -> Option<CodeActionResponse> {
    lsp_request::<CodeActionRequest>(
        service,
        CodeActionParams {
            text_document: TextDocumentIdentifier { uri },
            range,
            context: CodeActionContext {
                diagnostics: Vec::new(),
                only: None,
                trigger_kind: None,
            },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await
}

/// Build the expected `CodeActionResponse` for a "Convert to raw string" action
/// with a single text edit on `test_uri()`.
fn make_raw_string_action(edit_range: Range, new_text: &str) -> CodeActionResponse {
    vec![CodeActionOrCommand::CodeAction(CodeAction {
        title: "Convert to raw string".into(),
        kind: Some(CodeActionKind::REFACTOR_REWRITE),
        edit: Some(WorkspaceEdit {
            changes: Some(std::collections::HashMap::from([(
                test_uri(),
                vec![TextEdit {
                    range: edit_range,
                    new_text: new_text.into(),
                }],
            )])),
            document_changes: None,
            change_annotations: None,
        }),
        diagnostics: None,
        command: None,
        is_preferred: None,
        disabled: None,
        data: None,
    })]
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_not_offered_on_plain_string() {
    // "hello" has no escapes - converting to r"hello" is just noise.
    let grammar = r#"
grammar { language: "test" }
rule program { "hello" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let range = Range::new(Position::new(2, 16), Position::new(2, 16));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    assert_eq!(result, None);
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_convert_escaped_backslash_to_raw() {
    // String with escaped backslash (regex pattern) - conversion is useful.
    // Source: "[a-z]+\\s*"  (\\  is an escape for a literal \)
    // Raw:    r"[a-z]+\s*"  (no hashes needed, no " in content)
    let grammar = "grammar { language: \"test\" }\nrule program { regexp(\"[a-z]+\\\\s*\") }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    let range = Range::new(Position::new(1, 23), Position::new(1, 23));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    // String literal spans line 1, col 22..34.
    assert_eq!(
        result,
        Some(make_raw_string_action(
            Range::new(Position::new(1, 22), Position::new(1, 34)),
            "r\"[a-z]+\\s*\"",
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_convert_string_with_quotes_uses_hashes() {
    // Source: "he said \"hi\""  (escaped quotes inside)
    // Raw:    r#"he said "hi""#  (1 hash needed for embedded ")
    let grammar = "grammar { language: \"test\" }\nrule program { \"he said \\\"hi\\\"\" }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    let range = Range::new(Position::new(1, 16), Position::new(1, 16));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    // String literal spans line 1, col 15..31.
    assert_eq!(
        result,
        Some(make_raw_string_action(
            Range::new(Position::new(1, 15), Position::new(1, 31)),
            "r#\"he said \"hi\"\"#",
        ))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_not_offered_on_semantic_escapes() {
    // Strings with \n, \t, \r, \0 can't be faithfully converted to raw
    // (they'd become literal two-char sequences instead of the special char).
    let grammar = r#"
grammar { language: "test" }
rule program { "hey\nthere" }
"#;
    let mut service = init(&[(test_uri(), grammar)]).await;

    let range = Range::new(Position::new(2, 16), Position::new(2, 16));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    assert_eq!(result, None);
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_not_offered_on_raw_string() {
    let grammar = "grammar { language: \"test\" }\nrule program { r\"hello\" }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    let range = Range::new(Position::new(1, 16), Position::new(1, 16));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    assert_eq!(result, None);
}

#[tokio::test(flavor = "current_thread")]
async fn code_action_not_offered_on_identifier() {
    let grammar =
        "grammar { language: \"test\" }\nrule program { identifier }\nrule identifier { \"x\" }\n";
    let mut service = init(&[(test_uri(), grammar)]).await;

    let range = Range::new(Position::new(1, 16), Position::new(1, 16));
    let result = code_actions_at(&mut service, test_uri(), range).await;

    assert_eq!(result, None);
}

// ---------------------------------------------------------------------------
// Import tests
// ---------------------------------------------------------------------------

struct ImportFixture {
    _dir: tempfile::TempDir,
    helper_path: std::path::PathBuf,
    grammar_path: std::path::PathBuf,
    grammar_text: String,
}

fn create_import_fixture() -> ImportFixture {
    let dir = tempfile::tempdir().unwrap();

    // Helper module:
    // line 1: fn commaSep(item: rule_t) -> rule_t {
    //         col 3 = "commaSep"
    // line 5: let PREC = { DEFAULT: 0, CALL: 1 }
    //         col 4 = "PREC"
    let helper_text = r#"
fn commaSep(item: rule_t) -> rule_t {
    seq(item, repeat(seq(",", item)))
}

let PREC = { DEFAULT: 0, CALL: 1 }
"#;
    let helper_path = dir.path().join("helpers.tsg");
    std::fs::write(&helper_path, helper_text).unwrap();

    let grammar_text = format!(
        r#"
let helpers = import("{}")
grammar {{ language: "test" }}
rule program {{ repeat(expression) }}
rule expression {{ helpers::commaSep("x") }}
"#,
        helper_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    ImportFixture {
        _dir: dir,
        helper_path,
        grammar_path,
        grammar_text,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_import_path() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();
    let helper_uri = Url::from_file_path(&fix.helper_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    let path_offset = fix
        .grammar_text
        .find(&fix.helper_path.display().to_string())
        .unwrap();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, path_offset as u32);

    let result = goto_def_at(&mut service, grammar_uri, pos).await;
    assert_eq!(
        result,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: helper_uri,
            range: Range::default(),
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_import_function() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();
    let helper_uri = Url::from_file_path(&fix.helper_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    // Cursor on "commaSep" in `helpers::commaSep("x")`
    let call_offset = fix.grammar_text.find("helpers::commaSep").unwrap() + "helpers::".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, call_offset as u32);

    let result = goto_def_at(&mut service, grammar_uri, pos).await;
    // "commaSep" is at line 1, col 3 in the helper file.
    assert_eq!(
        result,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: helper_uri,
            range: Range::new(Position::new(1, 3), Position::new(1, 11)),
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_import_function() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    let call_offset = fix.grammar_text.find("helpers::commaSep").unwrap() + "helpers::".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, call_offset as u32);

    let result = hover_at(&mut service, grammar_uri, pos).await;
    assert_eq!(
        result,
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: "```\nfn commaSep(item: rule_t) -> rule_t\n```".into(),
            }),
            range: None,
        })
    );
}

#[tokio::test(flavor = "current_thread")]
async fn completion_import_members_after_double_colon() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    let colon_offset = fix.grammar_text.find("helpers::commaSep").unwrap() + "helpers::".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, colon_offset as u32);

    let mut items = completions_at(&mut service, grammar_uri, pos).await;
    items.sort_by(|a, b| a.label.cmp(&b.label));

    assert_eq!(
        items,
        vec![
            CompletionItem {
                label: "PREC".into(),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some("let PREC".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "commaSep".into(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some("fn commaSep(item: rule_t) -> rule_t".into()),
                ..Default::default()
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn document_symbol_import_shows_as_module() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: grammar_uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let DocumentSymbolResponse::Nested(symbols) = result.unwrap() else {
        panic!("expected nested symbols");
    };

    let helpers_sym = symbols.iter().find(|s| s.name == "helpers").unwrap();
    assert_eq!(helpers_sym.kind, SymbolKind::MODULE);
}

// Verify that the inherit fixture's `base` let binding is still classified
// as a variable (not a module) in document symbols since inherit is not import.
#[tokio::test(flavor = "current_thread")]
async fn document_symbol_inherit_binding_is_variable() {
    let fix = create_inherit_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    let result: Option<DocumentSymbolResponse> = lsp_request::<DocumentSymbolRequest>(
        &mut service,
        DocumentSymbolParams {
            text_document: TextDocumentIdentifier { uri: derived_uri },
            work_done_progress_params: WorkDoneProgressParams::default(),
            partial_result_params: PartialResultParams::default(),
        },
    )
    .await;

    let DocumentSymbolResponse::Nested(symbols) = result.unwrap() else {
        panic!("expected nested symbols");
    };

    // `let base = inherit(...)` should still be VARIABLE, not MODULE.
    let base_sym = symbols.iter().find(|s| s.name == "base").unwrap();
    assert_eq!(base_sym.kind, SymbolKind::VARIABLE);
}

#[tokio::test(flavor = "current_thread")]
async fn goto_def_nested_import() {
    // Setup: grammar imports helpers, helpers imports utils.
    // grammar uses helpers::utils_fn which is actually utils::utils_fn.
    let dir = tempfile::tempdir().unwrap();

    // utils.tsg: defines utils_fn
    let utils_text = r#"
fn utils_fn(x: rule_t) -> rule_t { x }
"#;
    let utils_path = dir.path().join("utils.tsg");
    std::fs::write(&utils_path, utils_text).unwrap();

    // helpers.tsg: imports utils
    let helpers_text = format!(
        r#"
let utils = import("{}")
fn helper_fn(x: rule_t) -> rule_t {{ utils::utils_fn(x) }}
"#,
        utils_path.display()
    );
    let helpers_path = dir.path().join("helpers.tsg");
    std::fs::write(&helpers_path, &helpers_text).unwrap();

    // grammar.tsg: imports helpers, uses helpers::helper_fn
    let grammar_text = format!(
        r#"
let h = import("{}")
grammar {{ language: "test" }}
rule program {{ h::helper_fn("x") }}
"#,
        helpers_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();
    let helpers_uri = Url::from_file_path(&helpers_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &grammar_text)]).await;

    // Goto-def on "helper_fn" in `h::helper_fn("x")` should jump to helpers.tsg.
    // "helper_fn" is at line 2, col 3 in helpers.tsg.
    let offset = grammar_text.find("h::helper_fn").unwrap() + "h::".len();
    let rope = ropey::Rope::from_str(&grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    assert_eq!(
        goto_def_at(&mut service, grammar_uri.clone(), pos).await,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: helpers_uri,
            range: Range::new(Position::new(2, 3), Position::new(2, 12)),
        }))
    );

    // Completion after `h::` should include both helper_fn and utils (the sub-import).
    let cc_pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);
    let mut items = completions_at(&mut service, grammar_uri, cc_pos).await;
    items.sort_by(|a, b| a.label.cmp(&b.label));

    assert_eq!(
        items,
        vec![
            CompletionItem {
                label: "helper_fn".into(),
                kind: Some(CompletionItemKind::FUNCTION),
                detail: Some("fn helper_fn(x: rule_t) -> rule_t".into()),
                ..Default::default()
            },
            CompletionItem {
                label: "utils".into(),
                kind: Some(CompletionItemKind::MODULE),
                detail: Some("import utils".into()),
                ..Default::default()
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn import_cycle_does_not_hang() {
    // Two files that import each other. The LSP should not hang or crash.
    let dir = tempfile::tempdir().unwrap();

    let a_path = dir.path().join("a.tsg");
    let b_path = dir.path().join("b.tsg");

    let a_text = format!(
        "let b = import(\"{}\")\nfn a_fn(x: rule_t) -> rule_t {{ x }}\n",
        b_path.display()
    );
    let b_text = format!(
        "let a = import(\"{}\")\nfn b_fn(x: rule_t) -> rule_t {{ x }}\n",
        a_path.display()
    );
    std::fs::write(&a_path, &a_text).unwrap();
    std::fs::write(&b_path, &b_text).unwrap();

    let grammar_text = format!(
        r#"
let moda = import("{}")
grammar {{ language: "test" }}
rule program {{ moda::a_fn("x") }}
"#,
        a_path.display()
    );
    let grammar_path = dir.path().join("grammar.tsg");
    std::fs::write(&grammar_path, &grammar_text).unwrap();

    let grammar_uri = Url::from_file_path(&grammar_path).unwrap();
    let a_uri = Url::from_file_path(&a_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &grammar_text)]).await;

    // Should still be able to goto-def on a_fn despite the cycle.
    // "a_fn" is at line 1, col 3 in a.tsg.
    let offset = grammar_text.find("moda::a_fn").unwrap() + "moda::".len();
    let rope = ropey::Rope::from_str(&grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    assert_eq!(
        goto_def_at(&mut service, grammar_uri, pos).await,
        Some(GotoDefinitionResponse::Scalar(Location {
            uri: a_uri,
            range: Range::new(Position::new(1, 3), Position::new(1, 7)),
        }))
    );
}

#[tokio::test(flavor = "current_thread")]
async fn references_imported_member() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    // Cursor on "commaSep" in `helpers::commaSep("x")`
    let offset = fix.grammar_text.find("helpers::commaSep").unwrap() + "helpers::".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    // Without declaration: just the usage in this file.
    let result = references_at(&mut service, grammar_uri.clone(), pos, false).await;
    assert_eq!(
        result,
        Some(vec![Location {
            uri: grammar_uri.clone(),
            range: Range::new(Position::new(4, 27), Position::new(4, 35)),
        }])
    );

    // With declaration: usage in grammar + definition in helper.
    let helper_uri = Url::from_file_path(&fix.helper_path).unwrap();
    let mut result = references_at(&mut service, grammar_uri, pos, true)
        .await
        .unwrap();
    result.sort_by(|a, b| {
        a.uri
            .as_str()
            .cmp(b.uri.as_str())
            .then(a.range.start.line.cmp(&b.range.start.line))
    });
    assert_eq!(
        result,
        vec![
            // grammar.tsg usage
            Location {
                uri: Url::from_file_path(&fix.grammar_path).unwrap(),
                range: Range::new(Position::new(4, 27), Position::new(4, 35)),
            },
            // helpers.tsg declaration - "commaSep" at line 1, col 3
            Location {
                uri: helper_uri,
                range: Range::new(Position::new(1, 3), Position::new(1, 11)),
            },
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn highlight_imported_member() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    // Cursor on "commaSep" in `helpers::commaSep("x")`
    let offset = fix.grammar_text.find("helpers::commaSep").unwrap() + "helpers::".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    assert_eq!(
        highlights_at(&mut service, grammar_uri, pos).await,
        Some(vec![DocumentHighlight {
            range: Range::new(Position::new(4, 27), Position::new(4, 35)),
            kind: Some(DocumentHighlightKind::READ),
        }])
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_import_variable() {
    let fix = create_import_fixture();
    let grammar_uri = Url::from_file_path(&fix.grammar_path).unwrap();

    let mut service = init(&[(grammar_uri.clone(), &fix.grammar_text)]).await;

    // Cursor on "helpers" in `let helpers = import("...")`
    let offset = fix.grammar_text.find("let helpers").unwrap() + "let ".len();
    let rope = ropey::Rope::from_str(&fix.grammar_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    assert_eq!(
        hover_at(&mut service, grammar_uri, pos).await,
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: "```\nimport helpers\n```".into(),
            }),
            range: None,
        })
    );
}

// ---------------------------------------------------------------------------
// grammar_config tests
// ---------------------------------------------------------------------------

fn create_grammar_config_fixture() -> InheritFixture {
    let dir = tempfile::tempdir().unwrap();

    let base_text = r#"
grammar { language: "base_lang" }
rule program { repeat(_statement) }
rule _statement { choice(expression, "x") }
rule expression { choice(identifier, number) }
rule identifier { regexp(r"[a-z]+") }
rule number { regexp(r"[0-9]+") }
"#;
    let base_path = dir.path().join("base.tsg");
    std::fs::write(&base_path, base_text).unwrap();

    let derived_text = format!(
        r#"
let base = inherit("{}")
grammar {{
    language: "derived_lang",
    inherits: base,
    extras: grammar_config(base).extras,
}}
rule new_rule {{ "new" }}
"#,
        base_path.display()
    );
    let derived_path = dir.path().join("derived.tsg");
    std::fs::write(&derived_path, &derived_text).unwrap();

    InheritFixture {
        _dir: dir,
        base_path,
        _base_text: base_text.to_string(),
        derived_path,
        derived_text,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn completion_grammar_config_dot_fields() {
    let fix = create_grammar_config_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor right after `grammar_config(base).` before "extras"
    let offset = fix
        .derived_text
        .find("grammar_config(base).extras")
        .unwrap()
        + "grammar_config(base).".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let mut items = completions_at(&mut service, derived_uri, pos).await;
    items.sort_by(|a, b| a.label.cmp(&b.label));

    let ci = |label: &str, ty: &str| CompletionItem {
        label: label.into(),
        kind: Some(CompletionItemKind::FIELD),
        detail: Some(format!("{label}: {ty}")),
        ..Default::default()
    };
    assert_eq!(
        items,
        vec![
            ci("conflicts", "list_list_rule_t"),
            ci("externals", "list_rule_t"),
            ci("extras", "list_rule_t"),
            ci("inline", "list_rule_t"),
            ci("precedences", "list_list_rule_t"),
            ci("reserved", "{ [context]: list_rule_t }"),
            ci("supertypes", "list_rule_t"),
            ci("word", "rule_t"),
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_grammar_config_builtin() {
    let fix = create_grammar_config_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor on "grammar_config" keyword
    let offset = fix.derived_text.find("grammar_config(base)").unwrap();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    let result = hover_at(&mut service, derived_uri, pos).await;
    let hover = result.unwrap();
    let HoverContents::Markup(markup) = hover.contents else {
        panic!("expected markup");
    };
    // Should show the grammar_config builtin docs
    assert_eq!(
        markup.value,
        ts_grammar_ls::hover_docs::GRAMMAR_CONFIG
    );
}

#[tokio::test(flavor = "current_thread")]
async fn hover_grammar_config_field_access() {
    let fix = create_grammar_config_fixture();
    let derived_uri = Url::from_file_path(&fix.derived_path).unwrap();

    let mut service = init(&[(derived_uri.clone(), &fix.derived_text)]).await;

    // Cursor on "extras" in `grammar_config(base).extras`
    let gc_offset = fix.derived_text.find("grammar_config(base).extras").unwrap();
    let offset = gc_offset + "grammar_config(base).".len();
    let rope = ropey::Rope::from_str(&fix.derived_text);
    let pos = ts_grammar_ls::text::offset_to_position(&rope, offset as u32);

    // Should show the same docs as hovering on `extras:` inside a grammar block.
    assert_eq!(
        hover_at(&mut service, derived_uri, pos).await,
        Some(Hover {
            contents: HoverContents::Markup(MarkupContent {
                kind: MarkupKind::Markdown,
                value: ts_grammar_ls::hover_docs::CFG_EXTRAS.into(),
            }),
            range: None,
        })
    );
}
