//! Doc IR for the formatter. Small algebraic representation of layout
//! decisions; the printer renders it to text using a width budget.
//!
//! Inspired by Wadler's "A prettier printer" and prettier's implementation,
//! pared down to the constructors we actually need.
//!
//! Storage: a `DocArena` holds a `Vec<DocNode>` and a flat pool of child
//! ids. Builders allocate into the arena and return `DocId` indices. This
//! keeps the IR cache-friendly (no per-node heap allocation, sequential
//! traversal) at the cost of threading the arena through builder calls.

/// Index into a `DocArena`. Always points at a valid node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocId(u32);

impl DocId {
    /// Internal accessor for the printer.
    #[must_use]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// One node of the Doc IR. Children are `DocId`s into the owning arena.
#[derive(Debug, Clone)]
pub enum DocNode {
    /// Literal text. Must not contain newlines (use `Line`).
    Text(String),
    /// Verbatim region (may contain newlines). Used for comments and for
    /// `// tsg-format: off ... on` passthrough.
    Raw(String),
    /// Unconditional newline + indent.
    Line,
    /// Break-if-group-wrapped: newline+indent when the enclosing `Group`
    /// is broken, single space when flat.
    SoftLine,
    /// Break-if-group-wrapped without the flat-space: newline+indent when
    /// broken, nothing when flat.
    SoftBreak,
    /// Try to render flat; if it doesn't fit the width budget, break all
    /// `SoftLine`/`SoftBreak` inside as newlines.
    Group(DocId),
    /// Like `Group` for the all-flat case, but when it doesn't fit, makes a
    /// per-`SoftLine` decision: each break point packs as much content as
    /// fits before wrapping. Used for object literals where multiple
    /// `key: value` pairs share a line.
    Fill(DocId),
    /// Render child with indent depth increased by one level.
    Indent(DocId),
    /// Sequence: children stored in `arena.children[start..start+len]`.
    Concat {
        start: u32,
        len: u32,
    },
    /// Conditional emission keyed off the enclosing `Group`'s break state:
    /// emit `broken` if the group is broken, otherwise `flat`. Common use is
    /// trailing-comma-when-wrapped: `if_broken(text(","), nil())`.
    IfBroken {
        broken: DocId,
        flat: DocId,
    },
}

/// Owning storage for a built Doc. Construct with `DocArena::new()`, build
/// via the alloc methods, render via `print::render`.
#[derive(Debug, Default)]
pub struct DocArena {
    pub(super) nodes: Vec<DocNode>,
    pub(super) children: Vec<DocId>,
}

impl DocArena {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn alloc(&mut self, node: DocNode) -> DocId {
        let id = DocId(u32::try_from(self.nodes.len()).expect("doc arena overflow"));
        self.nodes.push(node);
        id
    }

    /// Empty doc (zero-length `Concat`). Useful when conditionally adding.
    pub fn nil(&mut self) -> DocId {
        self.alloc(DocNode::Concat { start: 0, len: 0 })
    }

    /// Verbatim text. Panics in debug builds if `s` contains newlines.
    pub fn text(&mut self, s: impl Into<String>) -> DocId {
        let s = s.into();
        debug_assert!(!s.contains('\n'), "Doc::text must not contain newlines");
        self.alloc(DocNode::Text(s))
    }

    /// Verbatim text region; may contain newlines.
    pub fn raw(&mut self, s: impl Into<String>) -> DocId {
        self.alloc(DocNode::Raw(s.into()))
    }

    /// Unconditional newline + indent.
    pub fn line(&mut self) -> DocId {
        self.alloc(DocNode::Line)
    }

    /// Break-if-group-wrapped, space-when-flat.
    pub fn softline(&mut self) -> DocId {
        self.alloc(DocNode::SoftLine)
    }

    /// Break-if-group-wrapped, empty-when-flat.
    pub fn softbreak(&mut self) -> DocId {
        self.alloc(DocNode::SoftBreak)
    }

    /// Group wrapper - try flat first, break softlines if doesn't fit.
    pub fn group(&mut self, child: DocId) -> DocId {
        self.alloc(DocNode::Group(child))
    }

    /// Fill wrapper - all-flat if it fits, otherwise pack as many segments
    /// per line as the width budget allows.
    pub fn fill(&mut self, child: DocId) -> DocId {
        self.alloc(DocNode::Fill(child))
    }

    /// Indent wrapper - increment indent depth inside.
    pub fn indent(&mut self, child: DocId) -> DocId {
        self.alloc(DocNode::Indent(child))
    }

    /// Emit `broken` if the enclosing group is broken, `flat` otherwise.
    /// Use `arena.nil()` for the empty side.
    pub fn if_broken(&mut self, broken: DocId, flat: DocId) -> DocId {
        self.alloc(DocNode::IfBroken { broken, flat })
    }

    /// Concatenate a slice of doc ids into a single node.
    pub fn concat(&mut self, parts: &[DocId]) -> DocId {
        if parts.is_empty() {
            return self.nil();
        }
        let start = u32::try_from(self.children.len()).expect("doc arena overflow");
        self.children.extend_from_slice(parts);
        let len = u32::try_from(parts.len()).expect("doc arena overflow");
        self.alloc(DocNode::Concat { start, len })
    }

    /// Look up a node by id. Panics on invalid id.
    pub(super) fn get(&self, id: DocId) -> &DocNode {
        &self.nodes[id.index()]
    }

    /// Look up a Concat node's children. Panics if `start+len` is out of
    /// range (which would indicate an arena bug).
    pub(super) fn concat_children(&self, start: u32, len: u32) -> &[DocId] {
        let lo = start as usize;
        let hi = lo + len as usize;
        &self.children[lo..hi]
    }
}
