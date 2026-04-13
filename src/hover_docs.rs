//! Hover documentation for DSL builtins, keywords, and types.
//!
//! Each entry is a Markdown string suitable for LSP hover responses.

// ---------------------------------------------------------------------------
// Combinators
// ---------------------------------------------------------------------------

pub const SEQ: &str = "\
```
seq(...items: rule_t) -> rule_t
```
Match items in sequence.
```
rule assignment {
    seq(field(name, identifier), \"=\", field(value, _expression))
}
```";

pub const CHOICE: &str = "\
```
choice(...items: rule_t) -> rule_t
```
Match exactly one of the items.
```
rule _literal {
    choice(string_literal, integer_literal, float_literal)
}
```";

pub const REPEAT: &str = "\
```
repeat(item: rule_t) -> rule_t
```
Match zero or more occurrences of `item`.
```
rule argument_list { seq(\"(\", repeat(seq(\",\", _expression)), \")\") }
```";

pub const REPEAT1: &str = "\
```
repeat1(item: rule_t) -> rule_t
```
Match one or more occurrences of `item`.
```
rule _statements { repeat1(_statement) }
```";

pub const OPTIONAL: &str = "\
```
optional(item: rule_t) -> rule_t
```
Match zero or one occurrence of `item`.
```
rule return_statement { seq(\"return\", optional(_expression)) }
```";

pub const BLANK: &str = "\
```
blank() -> rule_t
```
Match nothing (epsilon). Useful as a `choice` alternative.
```
rule _semicolon { choice(\";\", blank()) }
```";

pub const FIELD: &str = "\
```
field(name, content: rule_t) -> rule_t
```
Assign a field name to `content`. The name is a bare identifier.
```
rule pair { seq(field(key, _expression), \":\", field(value, _expression)) }
```";

pub const ALIAS: &str = "\
```
alias(content: rule_t, target) -> rule_t
```
Rename `content` in the syntax tree. If `target` is a bare name, produces \
a named alias; if a string, produces an anonymous alias.
```
rule import { seq(\"import\", alias(_string, module_name)) }
rule plus { alias(\"+\", \"add\") }
```";

pub const TOKEN: &str = "\
```
token(content: rule_t) -> rule_t
```
Mark `content` as a single indivisible token. The parser treats the entire \
content as one terminal, and extras (whitespace, comments) are not allowed \
between its parts.
```
rule float { token(seq(regexp(\"[0-9]+\"), \".\", regexp(\"[0-9]+\"))) }
```";

pub const TOKEN_IMMEDIATE: &str = "\
```
token_immediate(content: rule_t) -> rule_t
```
Like `token`, but also requires that no extras appear *before* this token. \
Used for tokens that must immediately follow the previous token.
```
rule bang { token_immediate(\"!\") }
```";

pub const PREC: &str = "\
```
prec(value: int_t | str_t, content: rule_t) -> rule_t
```
Set the precedence of `content`. Higher values bind tighter. \
A string value refers to a named precedence level from `precedences`.
```
rule binary_expression {
    prec_left(1, seq(_expression, \"+\", _expression))
}
rule member_expression {
    prec(\"member\", seq(_expression, \".\", identifier))
}
```";

pub const PREC_LEFT: &str = "\
```
prec_left(value: int_t | str_t, content: rule_t) -> rule_t
```
Set left-associative precedence. When two alternatives have the same \
precedence, the left one wins.
```
rule binary_expression {
    prec_left(1, seq(field(left, _expression), \"+\", field(right, _expression)))
}
```";

pub const PREC_RIGHT: &str = "\
```
prec_right(value: int_t | str_t, content: rule_t) -> rule_t
```
Set right-associative precedence. When two alternatives have the same \
precedence, the right one wins.
```
rule assignment {
    prec_right(0, seq(_expression, \"=\", _expression))
}
```";

pub const PREC_DYNAMIC: &str = "\
```
prec_dynamic(value: int_t, content: rule_t) -> rule_t
```
Set dynamic precedence, resolved at parse time via the GLR algorithm. \
Only accepts integer values (not named precedences).
```
rule primary_expression {
    prec_dynamic(-1, seq(\"(\", _expression, \")\"))
}
```";

pub const RESERVED: &str = "\
```
reserved(context: str_t, content: rule_t) -> rule_t
```
Apply reserved word handling for the given `context`. The context name \
must match a key in the grammar's `reserved` block.
```
rule _property_name { reserved(\"properties\", identifier) }
```";

pub const REGEXP: &str = "\
```
regexp(pattern: str_t, flags?: str_t) -> rule_t
```
Match a regular expression. Uses Rust regex syntax. The optional `flags` \
argument sets regex flags (e.g. `\"i\"` for case-insensitive).
```
rule identifier { regexp(r\"[a-zA-Z_][a-zA-Z0-9_]*\") }
rule comment { token(seq(\"//\", regexp(r\"[^\\n]*\"))) }
```";

pub const CONCAT: &str = "\
```
concat(...parts: str_t) -> str_t
```
Concatenate strings at compile time. Useful for building regex patterns \
from parts.
```
fn preprocessor(command: str_t) -> rule_t {
    alias(regexp(concat(\"#[ \\t]*\", command)), concat(\"#\", command))
}
```";

pub const APPEND: &str = "\
```
append(left: list_rule_t, right: list_rule_t) -> list_rule_t
```
Concatenate two lists. Both operands must be lists of the same type.
```
let base_extras: list_rule_t = [regexp(r\"\\s\"), comment]
grammar { extras: append(base_extras, [line_continuation]) }
```";

pub const INHERIT: &str = "\
```
inherit(path: str_t) -> grammar
```
Inherit rules and configuration from a base grammar at `path` (relative \
to this file's directory). Used with `override rule` and config access \
(`base.extras`, `base::rule_name`).
```
let base = inherit(\"../tree-sitter-c/grammar.tsg\")
grammar { inherits: base, extras: base.extras }
override rule expression { choice(base::expression, new_variant) }
```";

// ---------------------------------------------------------------------------
// Keywords
// ---------------------------------------------------------------------------

pub const KW_GRAMMAR: &str = "\
```
grammar { ... }
```
The grammar configuration block. Defines the language name, extras, \
externals, conflicts, precedences, word token, and other settings.
```
grammar {
    language: \"javascript\",
    extras: [regexp(r\"\\s\"), comment],
    word: identifier,
}
```";

pub const KW_RULE: &str = "\
```
rule <name> { <body> }
```
Define a grammar rule. The body is a rule expression.
```
rule program { repeat(_statement) }
rule _expression { choice(binary_expression, identifier, number) }
```";

pub const KW_OVERRIDE: &str = "\
```
override rule <name> { <body> }
```
Override an inherited rule from the base grammar. Only valid in grammars \
that use `inherit()`.
```
override rule expression {
    choice(base::expression, lambda_expression)
}
```";

pub const KW_LET: &str = "\
```
let <name>[: <type>] = <value>
```
Bind a value to a name. The type annotation is optional (inferred from \
the value), except for empty lists which require an explicit type.
```
let PREC = { ADD: 1, MUL: 2 }
let extras: list_rule_t = [regexp(r\"\\s\"), comment]
```";

pub const KW_FN: &str = "\
```
fn <name>(<params>) -> <return_type> { <body> }
```
Define a function. Parameters require type annotations. The return type \
is required. Functions can be called from rule bodies and other functions.
```
fn commaSep1(item: rule_t) -> rule_t {
    seq(item, repeat(seq(\",\", item)))
}
fn commaSep(item: rule_t) -> rule_t { optional(commaSep1(item)) }
```";

pub const KW_FOR: &str = "\
```
for (<bindings>) in <iterable> { <body> }
```
Iterate over a list of tuples, producing a `choice` of the body for each \
element.
```
let ops: list_rule_t = [(\"&&\", 2), (\"||\", 1)]
rule binary_expr {
    choice(for (op: str_t, p: int_t) in ops {
        prec_left(p, seq(_expression, op, _expression))
    })
}
```";

pub const KW_PRINT: &str = "\
```
print(<expr>)
```
Debug-print a value to stderr at grammar evaluation time. The argument can \
be any concrete value (a rule, string, int, list, object, etc.). Output is \
prefixed with `path:line:`. Passing a bare rule reference expands the rule \
body. Only valid as a top-level item. Does not produce a value - cannot be \
bound, spread, or composed.
```
let PREC = { ADD: 1, MUL: 2 }
print(PREC)
// grammar.tsg:3: {
//   ADD: 1,
//   MUL: 2
// }
```";

// ---------------------------------------------------------------------------
// Grammar config fields
// ---------------------------------------------------------------------------

pub const CFG_LANGUAGE: &str = "\
```
language: str_t
```
The name of the language this grammar defines. Used as the identifier \
in tree-sitter API calls.";

pub const CFG_WORD: &str = "\
```
word: rule_t
```
The word token rule. This rule is used to handle keyword extraction - \
identifiers that match a keyword are promoted to that keyword's node type.
```
grammar { word: identifier }
```";

pub const CFG_EXTRAS: &str = "\
```
extras: list_rule_t
```
Rules that can appear anywhere in the document (e.g. whitespace, comments). \
Defaults to `[regexp(r\"\\s\")]` if not specified.
```
grammar { extras: [regexp(r\"\\s\"), comment] }
```";

pub const CFG_EXTERNALS: &str = "\
```
externals: list_rule_t
```
Tokens produced by an external scanner (custom C code). These are declared \
here and referenced in rules like any other token.
```
grammar { externals: [heredoc_start, heredoc_body, heredoc_end] }
```";

pub const CFG_SUPERTYPES: &str = "\
```
supertypes: list_rule_t
```
Abstract rules that serve as categories for other rules. This metadata \
is included in the generated `node-types.json` for tooling.
```
grammar { supertypes: [_expression, _statement, _declaration] }
```";

pub const CFG_INLINE: &str = "\
```
inline: list_rule_t
```
Rules whose bodies are inlined into their references at parse table \
generation time. Used to reduce state count.
```
grammar { inline: [_type_identifier, _field_identifier] }
```";

pub const CFG_CONFLICTS: &str = "\
```
conflicts: [[rule, ...], ...]
```
Sets of rules that are allowed to conflict (be ambiguous). Without this, \
the parser generator reports an error for any LR conflict.
```
grammar { conflicts: [[type_specifier, expression], [_declaration, statement]] }
```";

pub const CFG_PRECEDENCES: &str = "\
```
precedences: [[rule_or_string, ...], ...]
```
Global precedence ordering. Each inner list is a tier, ordered from highest \
to lowest precedence. Rules and named precedence strings can be mixed.
```
grammar { precedences: [[member_expression, call_expression], [unary_expression]] }
```";

pub const CFG_RESERVED: &str = "\
```
reserved: { context: list_rule_t, ... }
```
Reserved word sets keyed by context name. Used with `reserved(context, rule)` \
to make certain identifiers act as keywords in specific contexts.
```
grammar { reserved: { default: [\"if\", \"else\"], properties: [] } }
```";

pub const CFG_INHERITS: &str = "\
```
inherits: grammar
```
The base grammar to inherit from. Must be a value from `inherit()`. \
The derived grammar inherits all rules and config from the base.
```
let base = inherit(\"../tree-sitter-c/grammar.tsg\")
grammar { inherits: base }
```";

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

pub const TYPE_RULE_T: &str = "\
`rule_t` - A grammar rule expression (seq, choice, repeat, string literal, etc.)";

pub const TYPE_STR_T: &str = "\
`str_t` - A string value. Subtypes `rule_t` (strings are valid rule expressions).";

pub const TYPE_INT_T: &str = "\
`int_t` - An integer value.";

pub const TYPE_LIST_RULE_T: &str = "\
`list_rule_t` - A list of rule expressions.";

pub const TYPE_LIST_STR_T: &str = "\
`list_str_t` - A list of strings. Subtypes `list_rule_t`.";

pub const TYPE_LIST_INT_T: &str = "\
`list_int_t` - A list of integers.";

pub const TYPE_LIST_LIST_RULE_T: &str = "\
`list_list_rule_t` - A list of lists of rule expressions. Used by `conflicts` and `precedences`.";

pub const TYPE_LIST_LIST_STR_T: &str = "\
`list_list_str_t` - A list of lists of strings. Subtypes `list_list_rule_t`.";

pub const TYPE_LIST_LIST_INT_T: &str = "\
`list_list_int_t` - A list of lists of integers.";

pub const TYPE_VOID_T: &str = "\
`void_t` - Internal type returned by `print`. Not usable as a type annotation; \
appears in error messages when a `print` call or for-loop expansion is used \
where a real value is expected.";

pub const TYPE_SPREAD_T: &str = "\
`spread_t` - Internal type produced by for-loop expressions. A for-loop \
doesn't produce a standalone value; it splices its iterations inline into the \
enclosing `seq`/`choice`/list. Not usable as a type annotation; appears in \
error messages when a for-loop is used where a concrete value is expected.";

// ---------------------------------------------------------------------------
// Lookup
// ---------------------------------------------------------------------------

#[must_use]
pub fn grammar_field_hover(name: &str) -> Option<&'static str> {
    Some(match name {
        "language" => CFG_LANGUAGE,
        "word" => CFG_WORD,
        "extras" => CFG_EXTRAS,
        "externals" => CFG_EXTERNALS,
        "supertypes" => CFG_SUPERTYPES,
        "inline" => CFG_INLINE,
        "conflicts" => CFG_CONFLICTS,
        "precedences" => CFG_PRECEDENCES,
        "reserved" => CFG_RESERVED,
        "inherits" => CFG_INHERITS,
        _ => return None,
    })
}

#[must_use]
pub fn builtin_hover(name: &str) -> Option<&'static str> {
    Some(match name {
        "seq" => SEQ,
        "choice" => CHOICE,
        "repeat" => REPEAT,
        "repeat1" => REPEAT1,
        "optional" => OPTIONAL,
        "blank" => BLANK,
        "field" => FIELD,
        "alias" => ALIAS,
        "token" => TOKEN,
        "token_immediate" => TOKEN_IMMEDIATE,
        "prec" => PREC,
        "prec_left" => PREC_LEFT,
        "prec_right" => PREC_RIGHT,
        "prec_dynamic" => PREC_DYNAMIC,
        "reserved" => RESERVED,
        "regexp" => REGEXP,
        "concat" => CONCAT,
        "append" => APPEND,
        "inherit" => INHERIT,
        "grammar" => KW_GRAMMAR,
        "rule" => KW_RULE,
        "override" => KW_OVERRIDE,
        "let" => KW_LET,
        "fn" | "in" => KW_FN,
        "for" => KW_FOR,
        "print" => KW_PRINT,
        "rule_t" => TYPE_RULE_T,
        "str_t" => TYPE_STR_T,
        "int_t" => TYPE_INT_T,
        "list_rule_t" => TYPE_LIST_RULE_T,
        "list_str_t" => TYPE_LIST_STR_T,
        "list_int_t" => TYPE_LIST_INT_T,
        "list_list_rule_t" => TYPE_LIST_LIST_RULE_T,
        "list_list_str_t" => TYPE_LIST_LIST_STR_T,
        "list_list_int_t" => TYPE_LIST_LIST_INT_T,
        "void_t" => TYPE_VOID_T,
        "spread_t" => TYPE_SPREAD_T,
        _ => return None,
    })
}
