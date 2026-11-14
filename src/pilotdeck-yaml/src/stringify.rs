// Port of yaml@2.9.0 (ISC) dist/stringify/{stringify,stringifyDocument,
// stringifyCollection,stringifyPair,stringifyString,stringifyNumber,
// stringifyComment,foldFlowLines}.js, the core-schema tag stringifiers, and the
// Document/Collection edit subset (setIn, deleteIn, createNode) that the
// baseline's updatePilotLocalSettings applies before `document.toString()`.
//
// Scope: documents parsed with the default (YAML 1.2 core) schema, default
// toString options. Shapes outside that scope — `%YAML`/custom `%TAG`
// directives, yaml-1.1 collections and scalars (set, omap, pairs, binary,
// timestamp, merge keys), lone UTF-16 surrogates — are reported as
// [`StringifyError::Unsupported`] instead of being rendered differently.
use crate::{
    doc::Document,
    nodes::{Node, NodeId, NodeKind, Pair, ScalarValue},
    schema::Resolve,
    value::{Container, Value, ValueGraph},
};
use pilotdeck_protocol::{JsString, JsValue};
use regex::Regex;
use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    sync::{Arc, LazyLock},
};

/// The `lineWidth` default of `toString` / `stringify`.
const LINE_WIDTH: usize = 80;
const MIN_CONTENT_WIDTH: usize = 20;
const INDENT_STEP: &str = "  ";
const DOUBLE_QUOTED_MIN_MULTI_LINE_LENGTH: usize = 40;

/// A failure of an edit or of stringification.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StringifyError {
    /// A JS `Error` the original throws, with its message.
    Error(String),
    /// A document shape this port does not render.
    Unsupported(String),
}

/// The `toString` / `stringify` options the baseline passes: `lineWidth`
/// (0 turns folding off) and `singleQuote` (`None` is the default `null`,
/// `Some(false)` always picks double quotes when a string needs quoting).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StringifyOptions {
    pub line_width: usize,
    pub single_quote: Option<bool>,
}

impl Default for StringifyOptions {
    fn default() -> Self {
        Self {
            line_width: LINE_WIDTH,
            single_quote: None,
        }
    }
}

/// A parsed document with the original's in-place edits applied.
pub struct EditableDocument {
    doc: Document,
    options: StringifyOptions,
    /// Nodes created by edits: they carry no `source`.
    created: HashSet<NodeId>,
    /// Pair values the original keeps as raw JS primitives (not nodes).
    raw: HashSet<NodeId>,
    /// `minFractionDigits` kept by a parsed float scalar whose value an edit
    /// replaced (the original mutates `scalar.value` in place).
    min_fraction: HashMap<NodeId, Option<usize>>,
}

impl EditableDocument {
    pub fn new(doc: Document) -> Self {
        Self {
            doc,
            options: StringifyOptions::default(),
            created: HashSet::new(),
            raw: HashSet::new(),
            min_fraction: HashMap::new(),
        }
    }

    pub fn document(&self) -> &Document {
        &self.doc
    }

    fn add(&mut self, node: Node) -> NodeId {
        self.doc.nodes.push(node);
        let id = self.doc.nodes.len() - 1;
        self.created.insert(id);
        id
    }

    /// `doc.createNode(value)` for JSON-shaped values: objects become block
    /// maps of scalar-keyed pairs (undefined values skipped), arrays become
    /// sequences, primitives scalars.
    fn create_node(&mut self, value: &JsValue) -> Result<NodeId, StringifyError> {
        let kind = match value {
            JsValue::Null | JsValue::Undefined => NodeKind::Scalar(ScalarValue::Null),
            JsValue::Bool(b) => NodeKind::Scalar(ScalarValue::Bool(*b)),
            JsValue::Number(n) => NodeKind::Scalar(ScalarValue::Number(*n)),
            JsValue::String(s) => NodeKind::Scalar(ScalarValue::String(Arc::new(s.clone()))),
            JsValue::Array(items) => {
                let mut ids = Vec::with_capacity(items.len());
                for item in items {
                    ids.push(self.create_node(item)?);
                }
                NodeKind::Seq(ids)
            }
            JsValue::Object(object) => {
                let mut pairs = Vec::new();
                for (key, item) in object.entries() {
                    if item.is_undefined() {
                        continue;
                    }
                    let key = self.add(Node::new(NodeKind::Scalar(ScalarValue::String(Arc::new(
                        key.clone(),
                    )))));
                    let value = self.create_node(item)?;
                    pairs.push(Pair {
                        key,
                        value: Some(value),
                    });
                }
                NodeKind::Map(pairs)
            }
            #[allow(unreachable_patterns)]
            _ => {
                return Err(StringifyError::Unsupported(
                    "unsupported JS value for createNode".into(),
                ));
            }
        };
        Ok(self.add(Node::new(kind)))
    }

    fn assert_collection(&self) -> Result<NodeId, StringifyError> {
        match self.doc.contents {
            Some(id) if is_collection(&self.doc.nodes[id]) => Ok(id),
            _ => Err(StringifyError::Error(
                "Expected a YAML collection as the document contents".into(),
            )),
        }
    }

    /// `doc.setIn(path, value)` with string keys.
    pub fn set_in(&mut self, path: &[&str], value: &JsValue) -> Result<(), StringifyError> {
        if path.is_empty() {
            let id = self.create_node(value)?;
            self.doc.contents = Some(id);
            return Ok(());
        }
        if self.doc.contents.is_none() {
            let id = self.collection_from_path(path, value)?;
            self.doc.contents = Some(id);
            return Ok(());
        }
        let root = self.assert_collection()?;
        self.collection_set_in(root, path, value)
    }

    /// `doc.deleteIn(path)` with string keys.
    pub fn delete_in(&mut self, path: &[&str]) -> Result<bool, StringifyError> {
        if path.is_empty() {
            return Ok(self.doc.contents.take().is_some());
        }
        let root = self.assert_collection()?;
        self.collection_delete_in(root, path)
    }

    /// `collectionFromPath(schema, path, value)`: nested maps around the
    /// value, all of them nodes.
    fn collection_from_path(
        &mut self,
        path: &[&str],
        value: &JsValue,
    ) -> Result<NodeId, StringifyError> {
        let mut current = self.create_node(value)?;
        for key in path.iter().rev() {
            let key = self.add(Node::new(NodeKind::Scalar(ScalarValue::String(Arc::new(
                JsString::from(*key),
            )))));
            current = self.add(Node::new(NodeKind::Map(vec![Pair {
                key,
                value: Some(current),
            }])));
        }
        Ok(current)
    }

    fn collection_set_in(
        &mut self,
        coll: NodeId,
        path: &[&str],
        value: &JsValue,
    ) -> Result<(), StringifyError> {
        let (key, rest) = (path[0], &path[1..]);
        if rest.is_empty() {
            return self.collection_set(coll, key, value);
        }
        match self.collection_get(coll, key)? {
            Some(node) if is_collection(&self.doc.nodes[node]) => {
                self.collection_set_in(node, rest, value)
            }
            None => {
                let created = self.collection_from_path(rest, value)?;
                self.collection_set_node(coll, key, created)
            }
            Some(_) => Err(expected_collection(key, rest)),
        }
    }

    fn collection_delete_in(
        &mut self,
        coll: NodeId,
        path: &[&str],
    ) -> Result<bool, StringifyError> {
        let (key, rest) = (path[0], &path[1..]);
        if rest.is_empty() {
            return self.collection_delete(coll, key);
        }
        match self.collection_get(coll, key)? {
            Some(node) if is_collection(&self.doc.nodes[node]) => {
                self.collection_delete_in(node, rest)
            }
            _ => Err(expected_collection(key, rest)),
        }
    }

    /// `findPair(items, key)` for a string key.
    fn find_pair(&self, pairs: &[Pair], key: &str) -> Option<usize> {
        pairs.iter().position(|pair| {
            matches!(&self.doc.nodes[pair.key].kind,
                NodeKind::Scalar(ScalarValue::String(s)) if s.equals_str(key))
        })
    }

    /// `collection.get(key, true)`; `None` is `undefined`.
    fn collection_get(&self, coll: NodeId, key: &str) -> Result<Option<NodeId>, StringifyError> {
        match &self.doc.nodes[coll].kind {
            NodeKind::Map(pairs) => Ok(self
                .find_pair(pairs, key)
                .and_then(|index| pairs[index].value)),
            NodeKind::Seq(items) => Ok(seq_index(key).and_then(|index| items.get(index).copied())),
            _ => Err(StringifyError::Unsupported("yaml-1.1 collection".into())),
        }
    }

    /// `YAMLMap.set(key, value)` / `YAMLSeq.set(key, value)` with a JS value.
    fn collection_set(
        &mut self,
        coll: NodeId,
        key: &str,
        value: &JsValue,
    ) -> Result<(), StringifyError> {
        if let NodeKind::Map(pairs) = &self.doc.nodes[coll].kind
            && let Some(index) = self.find_pair(pairs, key)
            && let Some(prev) = pairs[index].value
            && matches!(self.doc.nodes[prev].kind, NodeKind::Scalar(_))
            && !matches!(value, JsValue::Object(_) | JsValue::Array(_))
        {
            // `prev.value.value = _pair.value`: the scalar node keeps its
            // style, format, comments, source and minFractionDigits.
            let min_fraction = self.min_fraction_digits(prev);
            self.min_fraction.insert(prev, min_fraction);
            self.doc.nodes[prev].kind = NodeKind::Scalar(scalar_value(value));
            return Ok(());
        }
        let node = self.create_node(value)?;
        if !matches!(value, JsValue::Object(_) | JsValue::Array(_)) {
            self.raw.insert(node);
        }
        self.collection_set_node(coll, key, node)
    }

    fn collection_set_node(
        &mut self,
        coll: NodeId,
        key: &str,
        node: NodeId,
    ) -> Result<(), StringifyError> {
        match &self.doc.nodes[coll].kind {
            NodeKind::Map(pairs) => {
                if let Some(index) = self.find_pair(pairs, key) {
                    if let NodeKind::Map(pairs) = &mut self.doc.nodes[coll].kind {
                        pairs[index].value = Some(node);
                    }
                    return Ok(());
                }
                let key = self.add(Node::new(NodeKind::Scalar(ScalarValue::String(Arc::new(
                    JsString::from(key),
                )))));
                if let NodeKind::Map(pairs) = &mut self.doc.nodes[coll].kind {
                    pairs.push(Pair {
                        key,
                        value: Some(node),
                    });
                }
                Ok(())
            }
            NodeKind::Seq(_) => {
                let Some(index) = seq_index(key) else {
                    return Err(StringifyError::Error(format!(
                        "Expected a valid index, not {key}."
                    )));
                };
                if let NodeKind::Seq(items) = &mut self.doc.nodes[coll].kind {
                    if index < items.len() {
                        items[index] = node;
                    } else {
                        return Err(StringifyError::Unsupported("sparse sequence".into()));
                    }
                }
                Ok(())
            }
            _ => Err(StringifyError::Unsupported("yaml-1.1 collection".into())),
        }
    }

    fn collection_delete(&mut self, coll: NodeId, key: &str) -> Result<bool, StringifyError> {
        match &self.doc.nodes[coll].kind {
            NodeKind::Map(pairs) => {
                let Some(index) = self.find_pair(pairs, key) else {
                    return Ok(false);
                };
                if let NodeKind::Map(pairs) = &mut self.doc.nodes[coll].kind {
                    pairs.remove(index);
                }
                Ok(true)
            }
            NodeKind::Seq(items) => {
                let Some(index) = seq_index(key).filter(|index| *index < items.len()) else {
                    return Ok(false);
                };
                if let NodeKind::Seq(items) = &mut self.doc.nodes[coll].kind {
                    items.remove(index);
                }
                Ok(true)
            }
            _ => Err(StringifyError::Unsupported("yaml-1.1 collection".into())),
        }
    }

    /// The core float tag's `minFractionDigits` (core/float.js resolve): a
    /// plain float source with a fraction ending in `0`.
    fn min_fraction_digits(&self, id: NodeId) -> Option<usize> {
        if let Some(stored) = self.min_fraction.get(&id) {
            return *stored;
        }
        if self.created.contains(&id) {
            return None;
        }
        let node = &self.doc.nodes[id];
        if !matches!(node.kind, NodeKind::Scalar(ScalarValue::Number(_))) || node.format.is_some() {
            return None;
        }
        let source = node.source.to_utf8().ok()?;
        let dot = source.find('.')?;
        if !source.ends_with('0') || !FLOAT.is_match(&source) {
            return None;
        }
        Some(source.len() - dot - 1)
    }

    /// `stringify(value, options)` (public-api.js): `new Document(value)` with
    /// the default `createNode` options, then `toString(options)`. Objects and
    /// arrays reached more than once (aliases and cycles the parse kept as
    /// one JS object) get anchors `a1`, `a2`, … in the order their second
    /// reference is met, and every later reference becomes an alias
    /// (`aliasDuplicateObjects`, doc/anchors.js `createNodeAnchors`).
    pub fn stringify_value(
        graph: &ValueGraph,
        options: StringifyOptions,
    ) -> Result<String, StringifyError> {
        let empty = crate::parse_document(&JsString::from(""), crate::Options::default());
        let mut ed = Self::new(empty);
        ed.options = options;
        let mut refs: HashMap<usize, GraphRef> = HashMap::new();
        let mut alias_order: Vec<usize> = Vec::new();
        let root = ed.create_from_graph(graph, &graph.root, &mut refs, &mut alias_order)?;
        // `setAnchors`: only after every node exists, so a cycle's anchor
        // lands on its (by then created) source node.
        for id in alias_order {
            if let Some(GraphRef {
                node: Some(node),
                anchor: Some(anchor),
            }) = refs.get(&id)
            {
                ed.doc.nodes[*node].anchor = Some(JsString::from(anchor.as_str()));
            }
        }
        ed.doc.contents = root;
        ed.to_yaml_string()
    }

    fn create_from_graph(
        &mut self,
        graph: &ValueGraph,
        value: &Value,
        refs: &mut HashMap<usize, GraphRef>,
        alias_order: &mut Vec<usize>,
    ) -> Result<Option<NodeId>, StringifyError> {
        let kind = match value {
            Value::Undefined => return Ok(None),
            Value::Null => NodeKind::Scalar(ScalarValue::Null),
            Value::Bool(b) => NodeKind::Scalar(ScalarValue::Bool(*b)),
            Value::Number(n) => NodeKind::Scalar(ScalarValue::Number(*n)),
            Value::String(s) => NodeKind::Scalar(ScalarValue::String(s.clone())),
            Value::Symbol { .. } => {
                return Err(StringifyError::Unsupported("symbol value".into()));
            }
            Value::Ref(id) => {
                if refs.contains_key(id) {
                    if refs[id].anchor.is_none() {
                        let next = alias_order.len() + 1;
                        if let Some(entry) = refs.get_mut(id) {
                            entry.anchor = Some(format!("a{next}"));
                        }
                        alias_order.push(*id);
                    }
                    let anchor = refs[id].anchor.clone().unwrap_or_default();
                    return Ok(Some(
                        self.add(Node::new(NodeKind::Alias(JsString::from(anchor.as_str())))),
                    ));
                }
                refs.insert(
                    *id,
                    GraphRef {
                        anchor: None,
                        node: None,
                    },
                );
                let kind = match &graph.nodes[*id] {
                    Container::Object(entries) => {
                        // `Object.entries` order: array-index keys first,
                        // ascending, then the rest in insertion order.
                        let mut ordered: Vec<&(JsString, Value)> = entries.iter().collect();
                        ordered.sort_by_key(|(key, _)| match key.array_index() {
                            Some(index) => (0, index),
                            None => (1, 0),
                        });
                        let mut pairs = Vec::new();
                        for (key, item) in ordered {
                            let Some(value) =
                                self.create_from_graph(graph, item, refs, alias_order)?
                            else {
                                continue;
                            };
                            let key = self.add(Node::new(NodeKind::Scalar(ScalarValue::String(
                                Arc::new(key.clone()),
                            ))));
                            pairs.push(Pair {
                                key,
                                value: Some(value),
                            });
                        }
                        NodeKind::Map(pairs)
                    }
                    Container::Array(items) => {
                        let mut ids = Vec::with_capacity(items.len());
                        for item in items {
                            // An undefined array item becomes a null scalar.
                            let id = match self.create_from_graph(graph, item, refs, alias_order)? {
                                Some(id) => id,
                                None => self.add(Node::new(NodeKind::Scalar(ScalarValue::Null))),
                            };
                            ids.push(id);
                        }
                        NodeKind::Seq(ids)
                    }
                    Container::Map(_) | Container::Set(_) => {
                        return Err(StringifyError::Unsupported("Map or Set value".into()));
                    }
                };
                let node = self.add(Node::new(kind));
                if let Some(entry) = refs.get_mut(id) {
                    entry.node = Some(node);
                }
                return Ok(Some(node));
            }
        };
        Ok(Some(self.add(Node::new(kind))))
    }

    /// `doc.toString()` with the default options.
    pub fn to_yaml_string(&self) -> Result<String, StringifyError> {
        self.check_supported()?;
        let printer = Printer {
            ed: self,
            anchors: RefCell::new(HashSet::new()),
        };
        printer.document()
    }

    fn check_supported(&self) -> Result<(), StringifyError> {
        let unsupported = |what: &str| Err(StringifyError::Unsupported(what.into()));
        if self.doc.schema.yaml11 || self.doc.directives.explicit {
            return unsupported("%YAML directive");
        }
        if self.doc.directives.tags.len() > 1 {
            return unsupported("%TAG directive");
        }
        let utf8 = |s: &JsString| s.to_utf8().is_ok();
        if !utf8(&self.doc.comment) || !utf8(&self.doc.comment_before) {
            return unsupported("lone surrogate");
        }
        let mut pending: Vec<NodeId> = self.doc.contents.into_iter().collect();
        let mut seen = HashSet::new();
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            let node = &self.doc.nodes[id];
            if !utf8(&node.comment)
                || !utf8(&node.comment_before)
                || node.anchor.as_ref().is_some_and(|a| !utf8(a))
                || node.tag.as_ref().is_some_and(|t| !utf8(t))
            {
                return unsupported("lone surrogate");
            }
            match &node.kind {
                NodeKind::Scalar(ScalarValue::String(s)) if !utf8(s) => {
                    return unsupported("lone surrogate");
                }
                NodeKind::Scalar(
                    ScalarValue::Binary(_) | ScalarValue::Timestamp(_) | ScalarValue::Merge,
                ) => return unsupported("yaml-1.1 scalar"),
                NodeKind::Alias(name) if !utf8(name) => return unsupported("lone surrogate"),
                NodeKind::Set(_) | NodeKind::OMap(_) | NodeKind::Pairs(_) => {
                    return unsupported("yaml-1.1 collection");
                }
                NodeKind::Seq(items) => pending.extend(items.iter().copied()),
                NodeKind::Map(pairs) => {
                    for pair in pairs {
                        pending.push(pair.key);
                        pending.extend(pair.value);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }
}

/// `{ anchor, node }` of `sourceObjects` in doc/anchors.js.
struct GraphRef {
    anchor: Option<String>,
    node: Option<NodeId>,
}

fn scalar_value(value: &JsValue) -> ScalarValue {
    match value {
        JsValue::Bool(b) => ScalarValue::Bool(*b),
        JsValue::Number(n) => ScalarValue::Number(*n),
        JsValue::String(s) => ScalarValue::String(Arc::new(s.clone())),
        _ => ScalarValue::Null,
    }
}

fn is_collection(node: &Node) -> bool {
    matches!(
        node.kind,
        NodeKind::Map(_)
            | NodeKind::Seq(_)
            | NodeKind::Set(_)
            | NodeKind::OMap(_)
            | NodeKind::Pairs(_)
    )
}

fn expected_collection(key: &str, rest: &[&str]) -> StringifyError {
    StringifyError::Error(format!(
        "Expected YAML collection at {key}. Remaining path: {}",
        rest.join(",")
    ))
}

/// `asItemIndex(key)` for a string key: `Number(key)` as a non-negative
/// integer.
fn seq_index(key: &str) -> Option<usize> {
    let trimmed = key.trim();
    let number: f64 = if trimmed.is_empty() {
        0.0
    } else {
        trimmed.parse().ok()?
    };
    (number.fract() == 0.0 && number >= 0.0 && number.is_finite()).then_some(number as usize)
}

static FLOAT: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^[-+]?(?:\.[0-9]+|[0-9]+\.[0-9]*)$").unwrap());
static BOOL: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(?:[Tt]rue|TRUE|[Ff]alse|FALSE)$").unwrap());
static NULL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(?:~|[Nn]ull|NULL)?$").unwrap());

// ───────────────────────────────────────────────────────────────── printing

#[derive(Clone, Default)]
struct Ctx {
    indent: String,
    in_flow: Option<bool>,
    implicit_key: bool,
    all_null_values: bool,
    indent_at_start: Option<usize>,
    force_block_indent: bool,
    actual_string: bool,
}

#[derive(Clone, Copy)]
enum Item {
    Node(NodeId),
    Pair(NodeId, Option<NodeId>),
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TagKind {
    Map,
    Seq,
    Str,
    Null,
    Bool,
    Int,
    IntOct,
    IntHex,
    FloatNaN,
    FloatExp,
    Float,
}

struct Printer<'a> {
    ed: &'a EditableDocument,
    anchors: RefCell<HashSet<String>>,
}

type R<T> = Result<T, StringifyError>;

fn jlen(s: &str) -> usize {
    s.encode_utf16().count()
}

fn utf8(s: &JsString) -> String {
    s.to_utf8().unwrap_or_default()
}

/// `stringifyComment`: `str.replace(/^(?!$)(?: $)?/gm, '#')`.
fn comment_string(s: &str) -> String {
    s.split('\n')
        .map(|line| {
            if line.is_empty() {
                String::new()
            } else if line == " " {
                "#".into()
            } else {
                format!("#{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `indentComment(comment, indent)`.
fn indent_comment(comment: &str, indent: &str) -> String {
    if !comment.is_empty() && comment.chars().all(|c| c == '\n') {
        return comment[1..].to_string();
    }
    if indent.is_empty() {
        return comment.to_string();
    }
    comment
        .split('\n')
        .map(|line| {
            if line.chars().all(|c| c == ' ') {
                line.to_string()
            } else {
                format!("{indent}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `lineComment(str, indent, comment)`.
fn line_comment(s: &str, indent: &str, comment: &str) -> String {
    if s.ends_with('\n') {
        indent_comment(comment, indent)
    } else if comment.contains('\n') {
        format!("\n{}", indent_comment(comment, indent))
    } else {
        format!("{}{}", if s.ends_with(' ') { "" } else { " " }, comment)
    }
}

/// `JSON.stringify(number)`, with `-0` kept.
fn js_number(n: f64) -> String {
    if n == 0.0 && n.is_sign_negative() {
        return "-0".into();
    }
    pilotdeck_protocol::stringify_json(&JsValue::Number(n))
        .ok()
        .flatten()
        .unwrap_or_else(|| "null".into())
}

/// `Number.prototype.toString(radix)` for a non-negative integer.
fn radix_string(mut n: u128, radix: u32) -> String {
    if n == 0 {
        return "0".into();
    }
    let mut digits = Vec::new();
    while n > 0 {
        digits.push(std::char::from_digit((n % radix as u128) as u32, radix).unwrap());
        n /= radix as u128;
    }
    digits.iter().rev().collect()
}

/// `Number.prototype.toExponential()` without a digit count.
fn to_exponential(n: f64) -> String {
    let s = format!("{n:e}");
    match s.split_once('e') {
        Some((mantissa, exp)) if !exp.starts_with('-') => format!("{mantissa}e+{exp}"),
        _ => s,
    }
}

fn contains_document_marker(s: &str) -> bool {
    s.split('\n')
        .any(|line| line.starts_with('%') || line.starts_with("---") || line.starts_with("..."))
}

impl Printer<'_> {
    fn node(&self, id: NodeId) -> &Node {
        &self.ed.doc.nodes[id]
    }

    // ── stringifyDocument
    fn document(&self) -> R<String> {
        let doc = &self.ed.doc;
        let mut lines: Vec<String> = Vec::new();
        let has_directives = doc.directives.doc_start;
        if has_directives {
            lines.push("---".into());
        }
        let mut ctx = Ctx::default();
        if !doc.comment_before.is_empty() {
            if lines.len() != 1 {
                lines.insert(0, String::new());
            }
            let cs = comment_string(&utf8(&doc.comment_before));
            lines.insert(0, indent_comment(&cs, ""));
        }
        let mut chomp_keep = false;
        let mut content_comment: Option<String> = None;
        if let Some(contents) = doc.contents {
            let node = self.node(contents);
            if node.space_before && has_directives {
                lines.push(String::new());
            }
            if !node.comment_before.is_empty() {
                let cs = comment_string(&utf8(&node.comment_before));
                lines.push(indent_comment(&cs, ""));
            }
            ctx.force_block_indent = !doc.comment.is_empty();
            if !node.comment.is_empty() {
                content_comment = Some(utf8(&node.comment));
            }
            let mut comment_done = false;
            let mut body = if content_comment.is_some() {
                self.stringify(
                    Item::Node(contents),
                    &mut ctx,
                    Some(&mut comment_done),
                    None,
                )?
            } else {
                self.stringify(
                    Item::Node(contents),
                    &mut ctx,
                    Some(&mut comment_done),
                    Some(&mut chomp_keep),
                )?
            };
            if comment_done {
                content_comment = None;
            }
            if let Some(comment) = &content_comment {
                body += &line_comment(&body, "", &comment_string(comment));
            }
            if (body.starts_with('|') || body.starts_with('>'))
                && lines.last().is_some_and(|l| l == "---")
            {
                let last = lines.len() - 1;
                lines[last] = format!("--- {body}");
            } else {
                lines.push(body);
            }
        } else {
            // `stringify(null)`: a created null scalar.
            lines.push("null".into());
        }
        if doc.directives.doc_end {
            if !doc.comment.is_empty() {
                let cs = comment_string(&utf8(&doc.comment));
                if cs.contains('\n') {
                    lines.push("...".into());
                    lines.push(indent_comment(&cs, ""));
                } else {
                    lines.push(format!("... {cs}"));
                }
            } else {
                lines.push("...".into());
            }
        } else {
            let mut dc = utf8(&doc.comment);
            if !dc.is_empty() && chomp_keep {
                dc = dc.trim_start_matches('\n').to_string();
            }
            if !dc.is_empty() {
                if (!chomp_keep || content_comment.is_some())
                    && lines.last().is_some_and(|l| !l.is_empty())
                {
                    lines.push(String::new());
                }
                lines.push(indent_comment(&comment_string(&dc), ""));
            }
        }
        Ok(lines.join("\n") + "\n")
    }

    // ── stringify.js
    fn stringify(
        &self,
        item: Item,
        ctx: &mut Ctx,
        on_comment: Option<&mut bool>,
        on_chomp_keep: Option<&mut bool>,
    ) -> R<String> {
        let id = match item {
            Item::Pair(key, value) => return self.pair(key, value, ctx, on_comment, on_chomp_keep),
            Item::Node(id) => id,
        };
        if let NodeKind::Alias(source) = &self.node(id).kind {
            let source = utf8(source);
            anchor_is_valid(&source)?;
            if !self.anchors.borrow().contains(&source) {
                return Err(StringifyError::Error(format!(
                    "Unresolved alias (the anchor must be set before the alias): {source}"
                )));
            }
            return Ok(if ctx.implicit_key {
                format!("*{source} ")
            } else {
                format!("*{source}")
            });
        }
        let tag = self.tag_kind(id)?;
        let props = self.props(id)?;
        if !props.is_empty() {
            ctx.indent_at_start = Some(ctx.indent_at_start.unwrap_or(0) + jlen(&props) + 1);
        }
        let s = self.tag_stringify(id, tag, ctx, on_comment, on_chomp_keep)?;
        if props.is_empty() {
            return Ok(s);
        }
        let scalar = matches!(self.node(id).kind, NodeKind::Scalar(_));
        Ok(if scalar || s.starts_with('{') || s.starts_with('[') {
            format!("{props} {s}")
        } else {
            format!("{props}\n{}{s}", ctx.indent)
        })
    }

    /// `stringifyProps`: every core tag is a default tag, so only an
    /// explicit node tag prints.
    fn props(&self, id: NodeId) -> R<String> {
        let node = self.node(id);
        let mut props = Vec::new();
        if let Some(anchor) = node.anchor.as_ref().filter(|a| !a.is_empty()) {
            let anchor = utf8(anchor);
            anchor_is_valid(&anchor)?;
            self.anchors.borrow_mut().insert(anchor.clone());
            props.push(format!("&{anchor}"));
        }
        if let Some(tag) = node.tag.as_ref().filter(|t| !t.is_empty()) {
            props.push(tag_string(&utf8(tag)));
        }
        Ok(props.join(" "))
    }

    /// `getTagObject(doc.schema.tags, node)` over the core schema.
    fn tag_kind(&self, id: NodeId) -> R<TagKind> {
        let node = self.node(id);
        if let Some(tag) = node.tag.as_ref().filter(|t| !t.is_empty()) {
            let matches: Vec<_> = self
                .ed
                .doc
                .schema
                .tags
                .iter()
                .filter(|t| tag.equals_str(t.name))
                .collect();
            if !matches.is_empty() {
                let chosen = matches
                    .iter()
                    .find(|t| t.format == node.format)
                    .unwrap_or(&matches[0]);
                return Ok(match (chosen.collection, chosen.resolve) {
                    (Some("map"), _) => TagKind::Map,
                    (Some("seq"), _) => TagKind::Seq,
                    (_, Resolve::String) => TagKind::Str,
                    (_, Resolve::Null) => TagKind::Null,
                    (_, Resolve::Bool) => TagKind::Bool,
                    (_, Resolve::Int(_, 8)) => TagKind::IntOct,
                    (_, Resolve::Int(_, 16)) => TagKind::IntHex,
                    (_, Resolve::Int(..)) => TagKind::Int,
                    (_, Resolve::FloatNaN) => TagKind::FloatNaN,
                    (_, Resolve::Float) if chosen.format == Some("EXP") => TagKind::FloatExp,
                    (_, Resolve::Float) => TagKind::Float,
                    _ => return Err(StringifyError::Unsupported("tag".into())),
                });
            }
        }
        Ok(match &node.kind {
            NodeKind::Map(_) => TagKind::Map,
            NodeKind::Seq(_) => TagKind::Seq,
            NodeKind::Scalar(ScalarValue::Null) => TagKind::Null,
            NodeKind::Scalar(ScalarValue::Bool(_)) => TagKind::Bool,
            NodeKind::Scalar(ScalarValue::String(_)) => TagKind::Str,
            NodeKind::Scalar(ScalarValue::Number(n)) => {
                let format = node.format;
                if n.fract() == 0.0 && n.is_finite() {
                    match format {
                        Some("OCT") => TagKind::IntOct,
                        Some("HEX") => TagKind::IntHex,
                        Some("EXP") => TagKind::FloatExp,
                        _ => TagKind::Int,
                    }
                } else if format == Some("EXP") {
                    TagKind::FloatExp
                } else {
                    TagKind::FloatNaN
                }
            }
            _ => return Err(StringifyError::Unsupported("node".into())),
        })
    }

    fn source(&self, id: NodeId) -> Option<String> {
        if self.ed.created.contains(&id) {
            return None;
        }
        Some(utf8(&self.node(id).source))
    }

    fn tag_stringify(
        &self,
        id: NodeId,
        tag: TagKind,
        ctx: &mut Ctx,
        on_comment: Option<&mut bool>,
        on_chomp_keep: Option<&mut bool>,
    ) -> R<String> {
        let node = self.node(id);
        let value = match &node.kind {
            NodeKind::Scalar(value) => Some(value.clone()),
            _ => None,
        };
        match tag {
            TagKind::Map => self.map(id, ctx, on_comment, on_chomp_keep),
            TagKind::Seq => self.seq(id, ctx, on_comment, on_chomp_keep),
            TagKind::Null => Ok(match self.source(id) {
                Some(source) if NULL.is_match(&source) => source,
                _ => "null".into(),
            }),
            TagKind::Bool => {
                let value = matches!(value, Some(ScalarValue::Bool(true)));
                if let Some(source) = self.source(id).filter(|s| !s.is_empty())
                    && BOOL.is_match(&source)
                {
                    let sv = source.starts_with(['t', 'T']);
                    if value == sv {
                        return Ok(source);
                    }
                }
                Ok(if value { "true" } else { "false" }.into())
            }
            TagKind::IntOct | TagKind::IntHex => {
                let (radix, prefix) = if tag == TagKind::IntOct {
                    (8, "0o")
                } else {
                    (16, "0x")
                };
                match value {
                    Some(ScalarValue::Number(n))
                        if n.fract() == 0.0 && n >= 0.0 && n.is_finite() =>
                    {
                        Ok(format!("{prefix}{}", radix_string(n as u128, radix)))
                    }
                    _ => self.number(id),
                }
            }
            TagKind::FloatExp => match value {
                Some(ScalarValue::Number(n)) if n.is_finite() => Ok(to_exponential(n)),
                _ => self.number(id),
            },
            TagKind::Int | TagKind::FloatNaN | TagKind::Float => self.number(id),
            TagKind::Str => {
                let mut ctx = Ctx {
                    actual_string: true,
                    ..ctx.clone()
                };
                let text = match value {
                    Some(ScalarValue::String(s)) => utf8(&s),
                    Some(ScalarValue::Number(n)) => js_number(n),
                    Some(ScalarValue::Bool(b)) => b.to_string(),
                    _ => "null".into(),
                };
                let comment = utf8(&node.comment);
                self.string(
                    &text,
                    node.style,
                    &comment,
                    &mut ctx,
                    on_comment,
                    on_chomp_keep,
                )
            }
        }
    }

    /// `stringifyNumber(node)`.
    fn number(&self, id: NodeId) -> R<String> {
        let node = self.node(id);
        // `typeof value === 'number' ? value : Number(value)` for the
        // finiteness check; the text is `JSON.stringify(value)` of the value
        // as stored (a string stays quoted).
        let (n, mut s) = match &node.kind {
            NodeKind::Scalar(ScalarValue::Number(n)) => (*n, js_number(*n)),
            NodeKind::Scalar(ScalarValue::Bool(b)) => (f64::from(u8::from(*b)), b.to_string()),
            NodeKind::Scalar(ScalarValue::Null) => (0.0, "null".into()),
            NodeKind::Scalar(ScalarValue::String(text)) => {
                let text = utf8(text);
                let trimmed = text.trim();
                let n = if trimmed.is_empty() {
                    0.0
                } else {
                    trimmed.parse().unwrap_or(f64::NAN)
                };
                let json =
                    pilotdeck_protocol::stringify_json(&JsValue::String(text.as_str().into()))
                        .ok()
                        .flatten()
                        .unwrap_or_default();
                (n, json)
            }
            _ => (f64::NAN, String::new()),
        };
        if !n.is_finite() {
            return Ok(if n.is_nan() {
                ".nan"
            } else if n < 0.0 {
                "-.inf"
            } else {
                ".inf"
            }
            .into());
        }
        let explicit_float = node
            .tag
            .as_ref()
            .is_none_or(|t| t.is_empty() || t.equals_str("tag:yaml.org,2002:float"));
        if let Some(digits) = self.ed.min_fraction_digits(id).filter(|d| *d > 0)
            && node.format.is_none()
            && explicit_float
            && s.strip_prefix('-')
                .unwrap_or(&s)
                .starts_with(|c: char| c.is_ascii_digit())
            && !s.contains('e')
        {
            let dot = match s.find('.') {
                Some(dot) => dot,
                None => {
                    s.push('.');
                    s.len() - 1
                }
            };
            let have = s.len() - dot - 1;
            for _ in have..digits {
                s.push('0');
            }
        }
        Ok(s)
    }

    // ── YAMLMap / YAMLSeq toString
    fn map(
        &self,
        id: NodeId,
        ctx: &mut Ctx,
        on_comment: Option<&mut bool>,
        on_chomp_keep: Option<&mut bool>,
    ) -> R<String> {
        let NodeKind::Map(pairs) = &self.node(id).kind else {
            return Err(StringifyError::Unsupported("map".into()));
        };
        let mut local;
        let ctx = if !ctx.all_null_values && pairs.iter().all(|p| p.value.is_none()) {
            local = Ctx {
                all_null_values: true,
                ..ctx.clone()
            };
            &mut local
        } else {
            ctx
        };
        let items: Vec<Item> = pairs.iter().map(|p| Item::Pair(p.key, p.value)).collect();
        let item_indent = ctx.indent.clone();
        self.collection(
            id,
            &items,
            ctx,
            "",
            ('{', '}'),
            item_indent,
            on_comment,
            on_chomp_keep,
        )
    }

    fn seq(
        &self,
        id: NodeId,
        ctx: &mut Ctx,
        on_comment: Option<&mut bool>,
        on_chomp_keep: Option<&mut bool>,
    ) -> R<String> {
        let NodeKind::Seq(ids) = &self.node(id).kind else {
            return Err(StringifyError::Unsupported("seq".into()));
        };
        let items: Vec<Item> = ids.iter().map(|id| Item::Node(*id)).collect();
        let item_indent = format!("{}  ", ctx.indent);
        self.collection(
            id,
            &items,
            ctx,
            "- ",
            ('[', ']'),
            item_indent,
            on_comment,
            on_chomp_keep,
        )
    }

    // ── stringifyCollection.js
    #[allow(clippy::too_many_arguments)]
    fn collection(
        &self,
        id: NodeId,
        items: &[Item],
        ctx: &mut Ctx,
        block_item_prefix: &str,
        flow_chars: (char, char),
        item_indent: String,
        on_comment: Option<&mut bool>,
        on_chomp_keep: Option<&mut bool>,
    ) -> R<String> {
        let flow = ctx.in_flow.unwrap_or(self.node(id).flow);
        if flow {
            self.flow_collection(items, ctx, flow_chars, item_indent)
        } else {
            self.block_collection(
                id,
                items,
                ctx,
                block_item_prefix,
                flow_chars,
                item_indent,
                on_comment,
                on_chomp_keep,
            )
        }
    }

    fn add_comment_before(
        &self,
        ctx: &Ctx,
        lines: &mut Vec<String>,
        comment: &JsString,
        chomp_keep: bool,
    ) {
        let mut comment = utf8(comment);
        if !comment.is_empty() && chomp_keep {
            comment = comment.trim_start_matches('\n').to_string();
        }
        if !comment.is_empty() {
            let ic = indent_comment(&comment_string(&comment), &ctx.indent);
            lines.push(ic.trim_start().to_string());
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn block_collection(
        &self,
        id: NodeId,
        items: &[Item],
        ctx: &mut Ctx,
        block_item_prefix: &str,
        flow_chars: (char, char),
        item_indent: String,
        on_comment: Option<&mut bool>,
        on_chomp_keep: Option<&mut bool>,
    ) -> R<String> {
        let indent = ctx.indent.clone();
        let mut item_ctx = Ctx {
            indent: item_indent.clone(),
            ..ctx.clone()
        };
        let mut chomp_keep = false;
        let mut lines: Vec<String> = Vec::new();
        for item in items {
            let mut comment: Option<String> = None;
            match *item {
                Item::Node(node_id) => {
                    let node = self.node(node_id);
                    if !chomp_keep && node.space_before {
                        lines.push(String::new());
                    }
                    self.add_comment_before(ctx, &mut lines, &node.comment_before, chomp_keep);
                    if !node.comment.is_empty() {
                        comment = Some(utf8(&node.comment));
                    }
                }
                Item::Pair(key, _) => {
                    let key = self.node(key);
                    if !chomp_keep && key.space_before {
                        lines.push(String::new());
                    }
                    self.add_comment_before(ctx, &mut lines, &key.comment_before, chomp_keep);
                }
            }
            chomp_keep = false;
            let mut comment_done = false;
            let mut s = self.stringify(
                *item,
                &mut item_ctx,
                Some(&mut comment_done),
                Some(&mut chomp_keep),
            )?;
            if comment_done {
                comment = None;
            }
            if let Some(comment) = &comment {
                s += &line_comment(&s, &item_indent, &comment_string(comment));
            }
            if chomp_keep && comment.is_some() {
                chomp_keep = false;
            }
            lines.push(format!("{block_item_prefix}{s}"));
        }
        let mut s = if lines.is_empty() {
            format!("{}{}", flow_chars.0, flow_chars.1)
        } else {
            let mut s = lines[0].clone();
            for line in &lines[1..] {
                if line.is_empty() {
                    s.push('\n');
                } else {
                    s += &format!("\n{indent}{line}");
                }
            }
            s
        };
        let comment = utf8(&self.node(id).comment);
        if !comment.is_empty() {
            s += &format!("\n{}", indent_comment(&comment_string(&comment), &indent));
            if let Some(flag) = on_comment {
                *flag = true;
            }
        } else if chomp_keep && let Some(flag) = on_chomp_keep {
            *flag = true;
        }
        Ok(s)
    }

    fn flow_collection(
        &self,
        items: &[Item],
        ctx: &mut Ctx,
        flow_chars: (char, char),
        item_indent: String,
    ) -> R<String> {
        let indent = ctx.indent.clone();
        let item_indent = format!("{item_indent}{INDENT_STEP}");
        let mut item_ctx = Ctx {
            indent: item_indent.clone(),
            in_flow: Some(true),
            ..ctx.clone()
        };
        let mut req_newline = false;
        let mut lines_at_value = 0;
        let mut lines: Vec<String> = Vec::new();
        for (i, item) in items.iter().enumerate() {
            let mut comment: Option<String> = None;
            match *item {
                Item::Node(node_id) => {
                    let node = self.node(node_id);
                    if node.space_before {
                        lines.push(String::new());
                    }
                    self.add_comment_before(ctx, &mut lines, &node.comment_before, false);
                    if !node.comment.is_empty() {
                        comment = Some(utf8(&node.comment));
                    }
                }
                Item::Pair(key, value) => {
                    let key = self.node(key);
                    if key.space_before {
                        lines.push(String::new());
                    }
                    self.add_comment_before(ctx, &mut lines, &key.comment_before, false);
                    if !key.comment.is_empty() {
                        req_newline = true;
                    }
                    match value.filter(|v| !self.ed.raw.contains(v)) {
                        Some(value) => {
                            let value = self.node(value);
                            if !value.comment.is_empty() {
                                comment = Some(utf8(&value.comment));
                            }
                            if !value.comment_before.is_empty() {
                                req_newline = true;
                            }
                        }
                        None => {
                            if value.is_none() && !key.comment.is_empty() {
                                comment = Some(utf8(&key.comment));
                            }
                        }
                    }
                }
            }
            if comment.is_some() {
                req_newline = true;
            }
            let mut comment_done = false;
            let mut s = self.stringify(*item, &mut item_ctx, Some(&mut comment_done), None)?;
            if comment_done {
                comment = None;
            }
            req_newline = req_newline || lines.len() > lines_at_value || s.contains('\n');
            if i < items.len() - 1 {
                s.push(',');
            }
            if let Some(comment) = &comment {
                s += &line_comment(&s, &item_indent, &comment_string(comment));
            }
            lines.push(s);
            lines_at_value = lines.len();
        }
        let (start, end) = flow_chars;
        if lines.is_empty() {
            return Ok(format!("{start}{end}"));
        }
        if !req_newline {
            let len = lines.iter().map(|l| jlen(l) + 2).sum::<usize>() + 2;
            let line_width = self.ed.options.line_width;
            req_newline = line_width > 0 && len > line_width;
        }
        if req_newline {
            let mut s = start.to_string();
            for line in &lines {
                if line.is_empty() {
                    s.push('\n');
                } else {
                    s += &format!("\n{INDENT_STEP}{indent}{line}");
                }
            }
            Ok(format!("{s}\n{indent}{end}"))
        } else {
            Ok(format!("{start} {} {end}", lines.join(" ")))
        }
    }

    // ── stringifyPair.js
    fn pair(
        &self,
        key: NodeId,
        value: Option<NodeId>,
        outer: &mut Ctx,
        on_comment: Option<&mut bool>,
        on_chomp_keep: Option<&mut bool>,
    ) -> R<String> {
        let all_null_values = outer.all_null_values;
        let indent = outer.indent.clone();
        let key_node = self.node(key);
        let mut key_comment = (!key_node.comment.is_empty()).then(|| utf8(&key_node.comment));
        let key_is_collection = is_collection(key_node);
        let mut explicit_key =
            (key_comment.is_some() && value.is_none() && outer.in_flow != Some(true))
                || key_is_collection
                || matches!(key_node.kind, NodeKind::Alias(_))
                || (matches!(key_node.kind, NodeKind::Scalar(_))
                    && matches!(key_node.style, "BLOCK_FOLDED" | "BLOCK_LITERAL"));
        let mut ctx = Ctx {
            all_null_values: false,
            implicit_key: !explicit_key && !all_null_values,
            indent: format!("{indent}{INDENT_STEP}"),
            ..outer.clone()
        };
        let mut key_comment_done = false;
        let mut chomp_keep = false;
        let mut s = self.stringify(
            Item::Node(key),
            &mut ctx,
            Some(&mut key_comment_done),
            Some(&mut chomp_keep),
        )?;
        if !explicit_key && ctx.in_flow != Some(true) && jlen(&s) > 1024 {
            explicit_key = true;
        }
        if ctx.in_flow == Some(true) {
            if all_null_values || value.is_none() {
                if key_comment_done && let Some(flag) = on_comment {
                    *flag = true;
                }
                return Ok(if s.is_empty() {
                    "?".into()
                } else if explicit_key {
                    format!("? {s}")
                } else {
                    s
                });
            }
        } else if all_null_values || (value.is_none() && explicit_key) {
            s = format!("? {s}");
            if let Some(comment) = &key_comment
                && !key_comment_done
            {
                s += &line_comment(&s, &ctx.indent, &comment_string(comment));
            } else if chomp_keep && let Some(flag) = on_chomp_keep {
                *flag = true;
            }
            return Ok(s);
        }
        if key_comment_done {
            key_comment = None;
        }
        if explicit_key {
            if let Some(comment) = &key_comment {
                s += &line_comment(&s, &ctx.indent, &comment_string(comment));
            }
            s = format!("? {s}\n{indent}:");
        } else {
            s.push(':');
            if let Some(comment) = &key_comment {
                s += &line_comment(&s, &ctx.indent, &comment_string(comment));
            }
        }
        let value_node = value.filter(|v| !self.ed.raw.contains(v));
        let (vsb, vcb, value_comment) = match value_node {
            Some(v) => {
                let n = self.node(v);
                (
                    n.space_before,
                    utf8(&n.comment_before),
                    (!n.comment.is_empty()).then(|| utf8(&n.comment)),
                )
            }
            None => (false, String::new(), None),
        };
        ctx.implicit_key = false;
        if !explicit_key
            && key_comment.is_none()
            && value_node.is_some_and(|v| matches!(self.node(v).kind, NodeKind::Scalar(_)))
        {
            ctx.indent_at_start = Some(jlen(&s) + 1);
        }
        chomp_keep = false;
        let mut value_comment_done = false;
        let value_str = match value {
            Some(v) => self.stringify(
                Item::Node(v),
                &mut ctx,
                Some(&mut value_comment_done),
                Some(&mut chomp_keep),
            )?,
            // `stringify(null)`: a created null scalar.
            None => "null".into(),
        };
        let mut ws = " ".to_string();
        if key_comment.is_some() || vsb || !vcb.is_empty() {
            ws = if vsb { "\n".into() } else { String::new() };
            if !vcb.is_empty() {
                ws += &format!("\n{}", indent_comment(&comment_string(&vcb), &ctx.indent));
            }
            if value_str.is_empty() && ctx.in_flow != Some(true) {
                if ws == "\n" && value_comment.is_some() {
                    ws = "\n\n".into();
                }
            } else {
                ws += &format!("\n{}", ctx.indent);
            }
        } else if !explicit_key && value.is_some_and(|v| is_collection(self.node(v))) {
            let v = self.node(value.unwrap());
            let empty = match &v.kind {
                NodeKind::Map(p) => p.is_empty(),
                NodeKind::Seq(i) => i.is_empty(),
                _ => false,
            };
            let vs0 = value_str.chars().next();
            let nl0 = value_str.find('\n');
            let has_newline = nl0.is_some();
            // `ctx.inFlow ?? value.flow ?? value.items.length === 0`: a block
            // collection's `flow` is undefined.
            let flow = match ctx.in_flow {
                Some(f) => f,
                None => v.flow || empty,
            };
            if has_newline || !flow {
                let mut has_props_line = false;
                if has_newline && matches!(vs0, Some('&' | '!')) {
                    let nl0 = nl0.unwrap();
                    let mut sp0 = value_str.find(' ');
                    if vs0 == Some('&')
                        && let Some(sp) = sp0
                        && sp < nl0
                        && value_str[sp + 1..].starts_with('!')
                    {
                        sp0 = value_str[sp + 1..].find(' ').map(|i| i + sp + 1);
                    }
                    if sp0.is_none_or(|sp| nl0 < sp) {
                        has_props_line = true;
                    }
                }
                if !has_props_line {
                    ws = format!("\n{}", ctx.indent);
                }
            }
        } else if value_str.is_empty() || value_str.starts_with('\n') {
            ws = String::new();
        }
        s += &ws;
        s += &value_str;
        if ctx.in_flow == Some(true) {
            if value_comment_done && let Some(flag) = on_comment {
                *flag = true;
            }
        } else if let Some(comment) = value_comment.filter(|_| !value_comment_done) {
            s += &line_comment(&s, &ctx.indent, &comment_string(&comment));
        } else if chomp_keep && let Some(flag) = on_chomp_keep {
            *flag = true;
        }
        Ok(s)
    }

    // ── stringifyString.js
    fn string(
        &self,
        value: &str,
        style: &str,
        comment: &str,
        ctx: &mut Ctx,
        on_comment: Option<&mut bool>,
        on_chomp_keep: Option<&mut bool>,
    ) -> R<String> {
        static CONTROL: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"[\x00-\x08\x0b-\x1f\x7f-\x9f]").unwrap());
        let mut style = style;
        if style != "QUOTE_DOUBLE" && CONTROL.is_match(value) {
            style = "QUOTE_DOUBLE";
        }
        let implicit_key = ctx.implicit_key;
        let in_flow = ctx.in_flow == Some(true);
        Ok(match style {
            "BLOCK_FOLDED" | "BLOCK_LITERAL" => {
                if implicit_key || in_flow {
                    self.quoted_string(value, ctx)
                } else {
                    self.block_string(value, style, comment, ctx, on_comment, on_chomp_keep)
                }
            }
            "QUOTE_DOUBLE" => self.double_quoted(value, ctx),
            "QUOTE_SINGLE" => self.single_quoted(value, ctx),
            // "PLAIN", and an unset type through defaultStringType 'PLAIN'
            // (the item keeps its unset type).
            _ => self.plain_string(value, style, comment, ctx, on_comment, on_chomp_keep),
        })
    }

    fn fold_options(&self, ctx: &Ctx, is_block: bool) -> Option<usize> {
        if is_block {
            Some(jlen(&ctx.indent))
        } else {
            ctx.indent_at_start
        }
    }

    fn double_quoted(&self, value: &str, ctx: &Ctx) -> String {
        let json: Vec<u16> = pilotdeck_protocol::stringify_json(&JsValue::String(value.into()))
            .ok()
            .flatten()
            .unwrap_or_default()
            .encode_utf16()
            .collect();
        let implicit_key = ctx.implicit_key;
        let indent = if !ctx.indent.is_empty() {
            ctx.indent.clone()
        } else if contains_document_marker(value) {
            "  ".into()
        } else {
            String::new()
        };
        let at = |i: usize| json.get(i).copied();
        let slice = |a: usize, b: usize| {
            String::from_utf16_lossy(
                &json[a.min(json.len())..b.min(json.len()).max(a.min(json.len()))],
            )
        };
        let mut out = String::new();
        let mut start = 0usize;
        let mut i = 0usize;
        while i < json.len() {
            let mut ch = json[i];
            if ch == b' ' as u16
                && at(i + 1) == Some(b'\\' as u16)
                && at(i + 2) == Some(b'n' as u16)
            {
                out += &slice(start, i);
                out += "\\ ";
                i += 1;
                start = i;
                ch = b'\\' as u16;
            }
            if ch == b'\\' as u16 {
                match at(i + 1).map(|c| c as u8 as char) {
                    Some('u') => {
                        out += &slice(start, i);
                        let code = slice(i + 2, i + 6);
                        match code.as_str() {
                            "0000" => out += "\\0",
                            "0007" => out += "\\a",
                            "000b" => out += "\\v",
                            "001b" => out += "\\e",
                            "0085" => out += "\\N",
                            "00a0" => out += "\\_",
                            "2028" => out += "\\L",
                            "2029" => out += "\\P",
                            _ => {
                                if let Some(rest) = code.strip_prefix("00") {
                                    out += &format!("\\x{rest}");
                                } else {
                                    out += &slice(i, i + 6);
                                }
                            }
                        }
                        i += 5;
                        start = i + 1;
                    }
                    Some('n') => {
                        if implicit_key
                            || at(i + 2) == Some(b'"' as u16)
                            || json.len() < DOUBLE_QUOTED_MIN_MULTI_LINE_LENGTH
                        {
                            i += 1;
                        } else {
                            out += &slice(start, i);
                            out += "\n\n";
                            while at(i + 2) == Some(b'\\' as u16)
                                && at(i + 3) == Some(b'n' as u16)
                                && at(i + 4) != Some(b'"' as u16)
                            {
                                out.push('\n');
                                i += 2;
                            }
                            out += &indent;
                            if at(i + 2) == Some(b' ' as u16) {
                                out.push('\\');
                            }
                            i += 1;
                            start = i + 1;
                        }
                    }
                    _ => i += 1,
                }
            }
            i += 1;
        }
        let s = if start > 0 {
            out + &slice(start, json.len())
        } else {
            String::from_utf16_lossy(&json)
        };
        if implicit_key {
            s
        } else {
            fold_flow_lines(
                &s,
                &indent,
                Fold::Quoted,
                self.fold_options(ctx, false),
                self.ed.options.line_width,
                None,
            )
        }
    }

    fn single_quoted(&self, value: &str, ctx: &Ctx) -> String {
        static WS_NEWLINE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"[ \t]\n|\n[ \t]").unwrap());
        static NEWLINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n+").unwrap());
        if self.ed.options.single_quote == Some(false)
            || (ctx.implicit_key && value.contains('\n'))
            || WS_NEWLINE.is_match(value)
        {
            return self.double_quoted(value, ctx);
        }
        let indent = if !ctx.indent.is_empty() {
            ctx.indent.clone()
        } else if contains_document_marker(value) {
            "  ".into()
        } else {
            String::new()
        };
        let escaped = value.replace('\'', "''");
        let escaped = NEWLINES.replace_all(&escaped, |c: &regex::Captures| {
            format!("{}\n{indent}", &c[0])
        });
        let res = format!("'{escaped}'");
        if ctx.implicit_key {
            res
        } else {
            fold_flow_lines(
                &res,
                &indent,
                Fold::Flow,
                self.fold_options(ctx, false),
                self.ed.options.line_width,
                None,
            )
        }
    }

    fn quoted_string(&self, value: &str, ctx: &Ctx) -> String {
        let has_double = value.contains('"');
        let has_single = value.contains('\'');
        if self.ed.options.single_quote == Some(false) {
            self.double_quoted(value, ctx)
        } else if has_double && !has_single {
            self.single_quoted(value, ctx)
        } else {
            // hasSingle && !hasDouble, or singleQuote null → double quotes.
            self.double_quoted(value, ctx)
        }
    }

    fn block_string(
        &self,
        value: &str,
        style: &str,
        comment: &str,
        ctx: &Ctx,
        on_comment: Option<&mut bool>,
        on_chomp_keep: Option<&mut bool>,
    ) -> String {
        static TRAILING_WS_LINE: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"\n[\t ]+$").unwrap());
        if TRAILING_WS_LINE.is_match(value) {
            return self.quoted_string(value, ctx);
        }
        let indent = if !ctx.indent.is_empty() {
            ctx.indent.clone()
        } else if ctx.force_block_indent || contains_document_marker(value) {
            "  ".into()
        } else {
            String::new()
        };
        let literal = if style == "BLOCK_FOLDED" {
            false
        } else if style == "BLOCK_LITERAL" {
            true
        } else {
            !line_length_over_limit(value, jlen(&indent), self.ed.options.line_width)
        };
        if value.is_empty() {
            return if literal { "|\n".into() } else { ">\n".into() };
        }
        let units: Vec<char> = value.chars().collect();
        let mut end_start = units.len();
        while end_start > 0 {
            let ch = units[end_start - 1];
            if ch != '\n' && ch != '\t' && ch != ' ' {
                break;
            }
            end_start -= 1;
        }
        let mut end: String = units[end_start..].iter().collect();
        let chomp;
        match end.find('\n') {
            None => chomp = "-",
            Some(pos) if value == end || pos != end.len() - 1 => {
                chomp = "+";
                if let Some(flag) = on_chomp_keep {
                    *flag = true;
                }
            }
            Some(_) => chomp = "",
        }
        let mut value: String = units[..end_start].iter().collect();
        if !end.is_empty() {
            if end.ends_with('\n') {
                end.pop();
            }
            end = block_end_newlines(&end, &indent);
        }
        let mut start_with_space = false;
        let mut start_end = 0;
        let mut start_nl_pos: isize = -1;
        let vchars: Vec<char> = value.chars().collect();
        while start_end < vchars.len() {
            match vchars[start_end] {
                ' ' => start_with_space = true,
                '\n' => start_nl_pos = start_end as isize,
                _ => break,
            }
            start_end += 1;
        }
        let take = if start_nl_pos < start_end as isize {
            (start_nl_pos + 1) as usize
        } else {
            start_end
        };
        let mut start: String = vchars[..take].iter().collect();
        if !start.is_empty() {
            value = vchars[take..].iter().collect();
            start = newlines_then(&start, &indent);
        }
        let indent_size = if indent.is_empty() { "1" } else { "2" };
        let mut header = format!("{}{chomp}", if start_with_space { indent_size } else { "" });
        if !comment.is_empty() {
            static CR_LF: LazyLock<Regex> = LazyLock::new(|| Regex::new(r" ?[\r\n]+").unwrap());
            header += &format!(" {}", comment_string(&CR_LF.replace_all(comment, " ")));
            if let Some(flag) = on_comment {
                *flag = true;
            }
        }
        if !literal {
            static MORE_INDENTED: LazyLock<Regex> = LazyLock::new(|| {
                Regex::new(r"(?:^|\n)([\t ].*)(?:([\n\t ]*)\n([^\n\t ]|$))?").unwrap()
            });
            static NEWLINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n+").unwrap());
            let folded = NEWLINES.replace_all(&value, |c: &regex::Captures| format!("\n{}", &c[0]));
            let folded = replace_more_indented(&MORE_INDENTED, &folded);
            let folded = newlines_then(&folded, &indent);
            let mut literal_fallback = false;
            let check_overflow = style != "BLOCK_FOLDED";
            let body = fold_flow_lines(
                &format!("{start}{folded}{end}"),
                &indent,
                Fold::Block,
                self.fold_options(ctx, true),
                self.ed.options.line_width,
                if check_overflow {
                    Some(&mut literal_fallback)
                } else {
                    None
                },
            );
            if !literal_fallback {
                return format!(">{header}\n{indent}{body}");
            }
        }
        let value = newlines_then(&value, &indent);
        format!("|{header}\n{indent}{start}{value}{end}")
    }

    fn plain_string(
        &self,
        value: &str,
        style: &str,
        comment: &str,
        ctx: &mut Ctx,
        on_comment: Option<&mut bool>,
        on_chomp_keep: Option<&mut bool>,
    ) -> String {
        static FLOW_INDICATORS: LazyLock<Regex> =
            LazyLock::new(|| Regex::new(r"[\[\]{},]").unwrap());
        static NOT_PLAIN: LazyLock<Regex> = LazyLock::new(|| {
            Regex::new(r##"^[\n\t ,\[\]{}#&*!|>'"%@`]|^[?-]$|^[?-][ \t]|[\n:][ \t]|[ \t]\n|[\n\t ]#|[\n\t :]$"##).unwrap()
        });
        let implicit_key = ctx.implicit_key;
        let in_flow = ctx.in_flow == Some(true);
        if (implicit_key && value.contains('\n')) || (in_flow && FLOW_INDICATORS.is_match(value)) {
            return self.quoted_string(value, ctx);
        }
        if NOT_PLAIN.is_match(value) {
            return if implicit_key || in_flow || !value.contains('\n') {
                self.quoted_string(value, ctx)
            } else {
                self.block_string(value, style, comment, ctx, on_comment, on_chomp_keep)
            };
        }
        if !implicit_key && !in_flow && style != "PLAIN" && value.contains('\n') {
            return self.block_string(value, style, comment, ctx, on_comment, on_chomp_keep);
        }
        if contains_document_marker(value) {
            if ctx.indent.is_empty() {
                ctx.force_block_indent = true;
                return self.block_string(value, style, comment, ctx, on_comment, on_chomp_keep);
            } else if implicit_key && ctx.indent == INDENT_STEP {
                return self.quoted_string(value, ctx);
            }
        }
        let s = newlines_then_newline(value, &ctx.indent);
        if ctx.actual_string {
            let parsed_as_other = self.ed.doc.schema.tags.iter().any(|tag| {
                (tag.default || tag.key_only)
                    && tag.name != "tag:yaml.org,2002:str"
                    && tag.test(&JsString::from(s.as_str()))
            });
            if parsed_as_other {
                return self.quoted_string(value, ctx);
            }
        }
        if implicit_key {
            s
        } else {
            fold_flow_lines(
                &s,
                &ctx.indent,
                Fold::Flow,
                self.fold_options(ctx, false),
                self.ed.options.line_width,
                None,
            )
        }
    }
}

fn anchor_is_valid(anchor: &str) -> R<()> {
    static INVALID: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"[\x00-\x19\s,\[\]{}]").unwrap());
    if INVALID.is_match(anchor) {
        let sa = pilotdeck_protocol::stringify_json(&JsValue::String(anchor.into()))
            .ok()
            .flatten()
            .unwrap_or_default();
        return Err(StringifyError::Error(format!(
            "Anchor must not contain whitespace or control characters: {sa}"
        )));
    }
    Ok(())
}

/// `doc.directives.tagString(tag)` over the default handles
/// (`!` → `!`, `!!` → `tag:yaml.org,2002:`).
fn tag_string(tag: &str) -> String {
    let escape = |name: &str| {
        name.chars()
            .map(|c| match c {
                '!' => "%21".to_string(),
                ',' => "%2C".to_string(),
                '[' => "%5B".to_string(),
                ']' => "%5D".to_string(),
                '{' => "%7B".to_string(),
                '}' => "%7D".to_string(),
                c => c.to_string(),
            })
            .collect::<String>()
    };
    if let Some(rest) = tag.strip_prefix('!') {
        return format!("!{}", escape(rest));
    }
    if let Some(rest) = tag.strip_prefix("tag:yaml.org,2002:") {
        return format!("!!{}", escape(rest));
    }
    format!("!<{tag}>")
}

/// `value.replace(/\n+/g, `$&${indent}`)`.
fn newlines_then(value: &str, indent: &str) -> String {
    static NEWLINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n+").unwrap());
    NEWLINES
        .replace_all(value, |c: &regex::Captures| format!("{}{indent}", &c[0]))
        .into_owned()
}

/// `value.replace(/\n+/g, `$&\n${indent}`)`.
fn newlines_then_newline(value: &str, indent: &str) -> String {
    static NEWLINES: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n+").unwrap());
    NEWLINES
        .replace_all(value, |c: &regex::Captures| format!("{}\n{indent}", &c[0]))
        .into_owned()
}

/// `end.replace(/(^|(?<!\n))\n+(?!\n|$)/g, `$&${indent}`)`: a maximal run of
/// newlines that does not reach the end of the string gets the indent.
fn block_end_newlines(end: &str, indent: &str) -> String {
    let chars: Vec<char> = end.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\n' {
            let mut j = i;
            while j < chars.len() && chars[j] == '\n' {
                j += 1;
            }
            out.extend(&chars[i..j]);
            if j < chars.len() {
                out += indent;
            }
            i = j;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

/// `.replace(/(?:^|\n)([\t ].*)(?:([\n\t ]*)\n(?![\n\t ]))?/g, '$1$2')`; the
/// lookahead is emulated by capturing the following character and putting
/// it back.
fn replace_more_indented(re: &Regex, s: &str) -> String {
    let mut out = String::new();
    let mut last = 0;
    let mut pos = 0;
    while pos <= s.len() {
        let Some(c) = re.captures_at(s, pos) else {
            break;
        };
        let m = c.get(0).unwrap();
        out += &s[last..m.start()];
        out += c.get(1).map_or("", |g| g.as_str());
        let mut end = m.end();
        if c.get(2).is_some() {
            out += c.get(2).map_or("", |g| g.as_str());
            if let Some(next) = c.get(3) {
                end = next.start();
            }
        } else if c.get(3).is_some() {
            end = c.get(3).unwrap().start();
        }
        last = end;
        pos = if end > m.start() { end } else { m.start() + 1 };
    }
    out += &s[last.min(s.len())..];
    out
}

fn line_length_over_limit(s: &str, indent_length: usize, line_width: usize) -> bool {
    if line_width == 0 {
        return false;
    }
    let units: Vec<u16> = s.encode_utf16().collect();
    let limit = line_width as isize - indent_length as isize;
    let len = units.len() as isize;
    if len <= limit {
        return false;
    }
    let mut start = 0isize;
    for (i, &c) in units.iter().enumerate() {
        if c == b'\n' as u16 {
            let i = i as isize;
            if i - start > limit {
                return true;
            }
            start = i + 1;
            if len - start <= limit {
                return false;
            }
        }
    }
    true
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Fold {
    Flow,
    Block,
    Quoted,
}

/// `foldFlowLines(text, indent, mode, { indentAtStart, lineWidth,
/// minContentWidth: 20, onOverflow })` over UTF-16 code units; a `lineWidth`
/// of 0 disables folding.
fn fold_flow_lines(
    text: &str,
    indent: &str,
    mode: Fold,
    indent_at_start: Option<usize>,
    line_width: usize,
    on_overflow: Option<&mut bool>,
) -> String {
    if line_width == 0 {
        return text.to_string();
    }
    let t: Vec<u16> = text.encode_utf16().collect();
    let indent_len = jlen(indent) as isize;
    let line_width = line_width as isize;
    let min_content_width = if line_width < MIN_CONTENT_WIDTH as isize {
        0
    } else {
        MIN_CONTENT_WIDTH as isize
    };
    let end_step = (1 + min_content_width).max(1 + line_width - indent_len);
    if t.len() as isize <= end_step {
        return text.to_string();
    }
    let at = |i: isize| -> Option<u16> {
        if i < 0 {
            None
        } else {
            t.get(i as usize).copied()
        }
    };
    let mut folds: Vec<isize> = Vec::new();
    let mut escaped_folds: HashSet<isize> = HashSet::new();
    let mut end = line_width - indent_len;
    if let Some(start) = indent_at_start {
        if start as isize > line_width - 2.max(min_content_width) {
            folds.push(0);
        } else {
            end = line_width - start as isize;
        }
    }
    let mut split: Option<isize> = None;
    let mut prev: Option<u16> = None;
    let mut overflow = false;
    let mut i: isize = -1;
    let mut esc_start: isize = -1;
    let mut esc_end: isize = -1;
    if mode == Fold::Block {
        i = consume_more_indented_lines(&t, i, indent_len);
        if i != -1 {
            end = i + end_step;
        }
    }
    let sp = b' ' as u16;
    let nl = b'\n' as u16;
    let tab = b'\t' as u16;
    loop {
        i += 1;
        let Some(mut ch) = at(i) else { break };
        if mode == Fold::Quoted && ch == b'\\' as u16 {
            esc_start = i;
            match at(i + 1).map(|c| c as u8 as char) {
                Some('x') => i += 3,
                Some('u') => i += 5,
                Some('U') => i += 9,
                _ => i += 1,
            }
            esc_end = i;
        }
        if ch == nl {
            if mode == Fold::Block {
                i = consume_more_indented_lines(&t, i, indent_len);
            }
            end = i + indent_len + end_step;
            split = None;
        } else {
            if ch == sp && prev.is_some_and(|p| p != sp && p != nl && p != tab) {
                let next = at(i + 1);
                if next.is_some_and(|n| n != sp && n != nl && n != tab) {
                    split = Some(i);
                }
            }
            if i >= end {
                if let Some(s) = split.filter(|s| *s != 0) {
                    folds.push(s);
                    end = s + end_step;
                    split = None;
                } else if mode == Fold::Quoted {
                    while prev == Some(sp) || prev == Some(tab) {
                        prev = Some(ch);
                        i += 1;
                        match at(i) {
                            Some(c) => ch = c,
                            None => {
                                ch = 0;
                                break;
                            }
                        }
                        overflow = true;
                    }
                    let j = if i > esc_end + 1 {
                        i - 2
                    } else {
                        esc_start - 1
                    };
                    if escaped_folds.contains(&j) {
                        return text.to_string();
                    }
                    folds.push(j);
                    escaped_folds.insert(j);
                    end = j + end_step;
                    split = None;
                } else {
                    overflow = true;
                }
            }
        }
        prev = if ch == 0 { None } else { Some(ch) };
        if ch == 0 {
            break;
        }
    }
    if overflow && let Some(flag) = on_overflow {
        *flag = true;
    }
    if folds.is_empty() {
        return text.to_string();
    }
    let slice = |a: isize, b: isize| -> String {
        let a = a.clamp(0, t.len() as isize) as usize;
        let b = b.clamp(0, t.len() as isize) as usize;
        String::from_utf16_lossy(&t[a..b.max(a)])
    };
    let mut res = slice(0, folds[0]);
    for (k, &fold) in folds.iter().enumerate() {
        let end = folds
            .get(k + 1)
            .copied()
            .filter(|f| *f != 0)
            .unwrap_or(t.len() as isize);
        if fold == 0 {
            res = format!("\n{indent}{}", slice(0, end));
        } else {
            if mode == Fold::Quoted && escaped_folds.contains(&fold) {
                res += &format!("{}\\", slice(fold, fold + 1));
            }
            res += &format!("\n{indent}{}", slice(fold + 1, end));
        }
    }
    res
}

fn consume_more_indented_lines(t: &[u16], mut i: isize, indent: isize) -> isize {
    let at = |i: isize| -> Option<u16> {
        if i < 0 {
            None
        } else {
            t.get(i as usize).copied()
        }
    };
    let mut end = i;
    let mut start = i + 1;
    let mut ch = at(start);
    while ch == Some(b' ' as u16) || ch == Some(b'\t' as u16) {
        if i < start + indent {
            i += 1;
            ch = at(i);
        } else {
            loop {
                i += 1;
                ch = at(i);
                if ch.is_none() || ch == Some(b'\n' as u16) {
                    break;
                }
            }
            end = i;
            start = i + 1;
            ch = at(start);
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Options, parse_document};

    fn edit(src: &str, f: impl FnOnce(&mut EditableDocument)) -> String {
        let doc = parse_document(
            &JsString::from(src),
            Options {
                pretty_errors: false,
                ..Options::default()
            },
        );
        assert!(doc.errors.is_empty());
        let mut ed = EditableDocument::new(doc);
        f(&mut ed);
        ed.to_yaml_string().unwrap()
    }

    /// Baseline output of `parseDocument(src)` + the same edits +
    /// `toString()` (yaml 2.9.0 under Node v22, captured for this test).
    #[test]
    fn local_settings_edit_matches_the_baseline_text() {
        let src = "schemaVersion: 1\nagent:\n  model: bbx/deterministic\n  maxContextTokens: 65536\n  maxOutputTokens: 8192\nmodel:\n  providers:\n    bbx:\n      models:\n        deterministic:\n          thinking:\n            efforts: [low, medium, high]\ntelemetry:\n  enabled: false\n";
        let out = edit(src, |ed| {
            ed.set_in(&["agent", "maxOutputTokens"], &JsValue::Number(4096.0))
                .unwrap();
            ed.set_in(
                &["agent", "subagents", "timeoutMs"],
                &JsValue::Number(60000.0),
            )
            .unwrap();
            ed.set_in(&["extension", "includeHookEvents"], &JsValue::Bool(true))
                .unwrap();
            ed.delete_in(&["agent", "maxOutputTokens"]).unwrap();
            ed.delete_in(&["extension", "includeHookEvents"]).unwrap();
        });
        assert_eq!(
            out,
            "schemaVersion: 1\nagent:\n  model: bbx/deterministic\n  maxContextTokens: 65536\n  subagents:\n    timeoutMs: 60000\nmodel:\n  providers:\n    bbx:\n      models:\n        deterministic:\n          thinking:\n            efforts: [ low, medium, high ]\ntelemetry:\n  enabled: false\nextension: {}\n"
        );
    }

    #[test]
    fn delete_through_a_missing_collection_throws_like_the_baseline() {
        let doc = parse_document(&JsString::from("agent: {}\n"), Options::default());
        let mut ed = EditableDocument::new(doc);
        assert_eq!(
            ed.delete_in(&["agent", "subagents", "default"]),
            Err(StringifyError::Error(
                "Expected YAML collection at subagents. Remaining path: default".into()
            ))
        );
    }
}
