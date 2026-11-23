//! DOM helpers on top of `markup5ever_rcdom`.
//!
//! Every stage between parsing and chapter splitting works on the same tree
//! (`Handle` = `Rc<Node>`). The functions here are the only DOM toolbox the
//! stages share, so they never panic and treat non-element nodes gracefully
//! (attribute getters return `None`, setters do nothing).
//!
//! [`serialize_xhtml`] and [`serialize_inner_xhtml`] always produce
//! well-formed XML: void elements are self-closed, attributes are quoted,
//! text is escaped, `U+00A0` (from `&nbsp;`) is written as `&#160;`, invalid
//! XML characters are dropped, comments/doctypes are omitted and SVG/MathML
//! subtrees carry their namespace declarations.

use std::cell::RefCell;
use std::rc::Rc;

use html5ever::tendril::{StrTendril, TendrilSink};
use html5ever::{Attribute, LocalName, ParseOpts, QualName, local_name, ns};
use markup5ever_rcdom::{NodeData, RcDom};

pub use markup5ever_rcdom::{Handle, Node};

pub const XHTML_NS: &str = "http://www.w3.org/1999/xhtml";
pub const SVG_NS: &str = "http://www.w3.org/2000/svg";
pub const MATHML_NS: &str = "http://www.w3.org/1998/Math/MathML";
pub const XLINK_NS: &str = "http://www.w3.org/1999/xlink";
pub const XML_NS: &str = "http://www.w3.org/XML/1998/namespace";

/// HTML void elements: serialized as `<name ... />`.
const VOID_ELEMENTS: &[&str] = &[
    "area", "base", "br", "col", "embed", "hr", "img", "input", "keygen", "link", "meta", "param",
    "source", "track", "wbr",
];

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parses an HTML document or fragment and returns the document node.
///
/// html5ever always builds `html > head + body`, so fragments (such as
/// Markdown output) end up inside `<body>`; use [`body`] to reach them.
/// Note that `<style>`, `<title>`, `<meta>` and `<link>` appearing before
/// any body content are placed in `<head>` (standard HTML parsing); the
/// sanitizer is expected to carry `<head>` styles over into the body.
pub fn parse(html: &str) -> Handle {
    let dom = html5ever::parse_document(RcDom::default(), ParseOpts::default()).one(html);
    dom.document
}

/// Parses an HTML fragment in a `<body>` context and returns its top-level
/// nodes, detached and ready to be inserted with [`append_child`],
/// [`insert_before`] or [`replace_with_many`].
pub fn parse_fragment(html: &str) -> Vec<Handle> {
    let dom = html5ever::parse_fragment(
        RcDom::default(),
        ParseOpts::default(),
        QualName::new(None, ns!(html), local_name!("body")),
        Vec::new(),
        false,
    )
    .one(html);
    // Fragment parsing yields Document > html > (nodes...).
    let root = dom
        .document
        .children
        .borrow()
        .iter()
        .find(|c| is_element(c))
        .cloned();
    let Some(root) = root else {
        return Vec::new();
    };
    let nodes: Vec<Handle> = root.children.borrow().clone();
    for n in &nodes {
        remove(n);
    }
    nodes
}

// ---------------------------------------------------------------------------
// Navigation and inspection
// ---------------------------------------------------------------------------

/// Whether the node is an element.
pub fn is_element(node: &Handle) -> bool {
    matches!(node.data, NodeData::Element { .. })
}

/// Whether the node is a text node.
pub fn is_text(node: &Handle) -> bool {
    matches!(node.data, NodeData::Text { .. })
}

/// Lowercase-insensitive local name of an element (`"p"`, `"svg"`,
/// `"foreignObject"`), or `None` for non-elements.
pub fn tag_name(node: &Handle) -> Option<String> {
    match &node.data {
        NodeData::Element { name, .. } => Some(name.local.to_string()),
        _ => None,
    }
}

/// Whether the node is an element with this local name (ASCII
/// case-insensitive).
pub fn is_tag(node: &Handle, tag: &str) -> bool {
    match &node.data {
        NodeData::Element { name, .. } => (*name.local).eq_ignore_ascii_case(tag),
        _ => false,
    }
}

/// Namespace URL of an element (`http://www.w3.org/1999/xhtml` for HTML).
pub fn namespace(node: &Handle) -> Option<String> {
    match &node.data {
        NodeData::Element { name, .. } => Some(name.ns.to_string()),
        _ => None,
    }
}

/// Snapshot of the node's children.
pub fn children(node: &Handle) -> Vec<Handle> {
    node.children.borrow().clone()
}

/// The node's parent, if attached.
pub fn parent(node: &Handle) -> Option<Handle> {
    let weak = node.parent.take();
    let parent = weak.as_ref().and_then(|w| w.upgrade());
    node.parent.set(weak);
    parent
}

/// All descendants in document order (excluding `node` itself).
pub fn descendants(node: &Handle) -> Vec<Handle> {
    let mut out = Vec::new();
    let mut stack: Vec<Handle> = node.children.borrow().iter().rev().cloned().collect();
    while let Some(n) = stack.pop() {
        stack.extend(n.children.borrow().iter().rev().cloned());
        out.push(n);
    }
    out
}

/// All descendant elements of `root` (excluding `root`) with the given local
/// name, ASCII case-insensitive, in document order. `"*"` matches every
/// element.
pub fn select_by_tag(root: &Handle, tag: &str) -> Vec<Handle> {
    descendants(root)
        .into_iter()
        .filter(|n| is_element(n) && (tag == "*" || is_tag(n, tag)))
        .collect()
}

/// First descendant element with the given local name.
pub fn first_by_tag(root: &Handle, tag: &str) -> Option<Handle> {
    let mut stack: Vec<Handle> = root.children.borrow().iter().rev().cloned().collect();
    while let Some(n) = stack.pop() {
        if is_tag(&n, tag) {
            return Some(n);
        }
        stack.extend(n.children.borrow().iter().rev().cloned());
    }
    None
}

/// The `<body>` element of a parsed document (or `root` itself if it is a
/// `body`).
pub fn body(root: &Handle) -> Option<Handle> {
    if is_tag(root, "body") {
        return Some(root.clone());
    }
    first_by_tag(root, "body")
}

/// Concatenated text of all descendant text nodes.
pub fn text_content(node: &Handle) -> String {
    let mut out = String::new();
    collect_text(node, &mut out);
    out
}

fn collect_text(node: &Handle, out: &mut String) {
    if let NodeData::Text { contents } = &node.data {
        out.push_str(&contents.borrow());
    }
    for c in node.children.borrow().iter() {
        collect_text(c, out);
    }
}

// ---------------------------------------------------------------------------
// Attributes
// ---------------------------------------------------------------------------

fn attr_matches(attr: &Attribute, name: &str) -> bool {
    if let Some((prefix, local)) = name.split_once(':') {
        if let Some(p) = &attr.name.prefix {
            return (**p).eq_ignore_ascii_case(prefix)
                && (*attr.name.local).eq_ignore_ascii_case(local);
        }
        // Unadjusted foreign attribute on an HTML element, e.g. "xlink:href".
        return (*attr.name.local).eq_ignore_ascii_case(name);
    }
    attr.name.prefix.is_none() && (*attr.name.local).eq_ignore_ascii_case(name)
}

/// Value of an attribute. `name` may be prefixed (`"xlink:href"`).
pub fn get_attr(node: &Handle, name: &str) -> Option<String> {
    match &node.data {
        NodeData::Element { attrs, .. } => attrs
            .borrow()
            .iter()
            .find(|a| attr_matches(a, name))
            .map(|a| a.value.to_string()),
        _ => None,
    }
}

/// Whether the element has the attribute.
pub fn has_attr(node: &Handle, name: &str) -> bool {
    get_attr(node, name).is_some()
}

/// Sets (or adds) an attribute. Does nothing on non-elements.
pub fn set_attr(node: &Handle, name: &str, value: &str) {
    if let NodeData::Element { attrs, .. } = &node.data {
        let mut attrs = attrs.borrow_mut();
        if let Some(a) = attrs.iter_mut().find(|a| attr_matches(a, name)) {
            a.value = StrTendril::from(value);
        } else {
            attrs.push(Attribute {
                name: attr_qualname(name),
                value: StrTendril::from(value),
            });
        }
    }
}

fn attr_qualname(name: &str) -> QualName {
    match name.split_once(':') {
        Some(("xlink", local)) => QualName::new(
            Some(html5ever::Prefix::from("xlink")),
            ns!(xlink),
            LocalName::from(local),
        ),
        Some(("xml", local)) => QualName::new(
            Some(html5ever::Prefix::from("xml")),
            ns!(xml),
            LocalName::from(local),
        ),
        _ => QualName::new(None, ns!(), LocalName::from(name)),
    }
}

/// Removes an attribute if present.
pub fn remove_attr(node: &Handle, name: &str) {
    if let NodeData::Element { attrs, .. } = &node.data {
        attrs.borrow_mut().retain(|a| !attr_matches(a, name));
    }
}

/// All attributes as `(name, value)` pairs; prefixed names come out as
/// `prefix:local`.
pub fn attrs(node: &Handle) -> Vec<(String, String)> {
    match &node.data {
        NodeData::Element { attrs, .. } => attrs
            .borrow()
            .iter()
            .map(|a| {
                let name = match &a.name.prefix {
                    Some(p) => format!("{}:{}", p, a.name.local),
                    None => a.name.local.to_string(),
                };
                (name, a.value.to_string())
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// Whether the element's `class` list contains `class`.
pub fn has_class(node: &Handle, class: &str) -> bool {
    get_attr(node, "class")
        .map(|c| c.split_ascii_whitespace().any(|c| c == class))
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Construction and mutation
// ---------------------------------------------------------------------------

/// Creates a detached HTML element with the given attributes.
pub fn create_element(tag: &str, attrs: &[(&str, &str)]) -> Handle {
    let node = Node::new(NodeData::Element {
        name: QualName::new(None, ns!(html), LocalName::from(tag)),
        attrs: RefCell::new(Vec::new()),
        template_contents: RefCell::new(None),
        mathml_annotation_xml_integration_point: false,
    });
    for (k, v) in attrs {
        set_attr(&node, k, v);
    }
    node
}

/// Creates a detached text node.
pub fn create_text(text: &str) -> Handle {
    Node::new(NodeData::Text {
        contents: RefCell::new(StrTendril::from(text)),
    })
}

/// Detaches `node` from its parent (no-op when already detached).
pub fn remove(node: &Handle) {
    if let Some(p) = parent(node) {
        p.children.borrow_mut().retain(|c| !Rc::ptr_eq(c, node));
    }
    node.parent.set(None);
}

/// Appends `child` as the last child of `parent`, detaching it first.
pub fn append_child(parent: &Handle, child: &Handle) {
    if Rc::ptr_eq(parent, child) {
        return;
    }
    remove(child);
    child.parent.set(Some(Rc::downgrade(parent)));
    parent.children.borrow_mut().push(child.clone());
}

/// Inserts `new` right before `reference` in its parent. Does nothing when
/// `reference` is detached.
pub fn insert_before(reference: &Handle, new: &Handle) {
    if Rc::ptr_eq(reference, new) {
        return;
    }
    let Some(p) = parent(reference) else {
        return;
    };
    remove(new);
    let mut kids = p.children.borrow_mut();
    let idx = kids
        .iter()
        .position(|c| Rc::ptr_eq(c, reference))
        .unwrap_or(kids.len());
    new.parent.set(Some(Rc::downgrade(&p)));
    kids.insert(idx, new.clone());
}

/// Replaces `old` with `new` in the tree. Does nothing when `old` is
/// detached.
pub fn replace_with(old: &Handle, new: &Handle) {
    replace_with_many(old, std::slice::from_ref(new));
}

/// Replaces `old` with several nodes (in order). Does nothing when `old` is
/// detached.
pub fn replace_with_many(old: &Handle, new: &[Handle]) {
    if parent(old).is_none() {
        return;
    }
    for n in new {
        if !Rc::ptr_eq(n, old) {
            insert_before(old, n);
        }
    }
    if !new.iter().any(|n| Rc::ptr_eq(n, old)) {
        remove(old);
    }
}

/// Replaces an element by its children (keeps the content, drops the tag).
pub fn unwrap(node: &Handle) {
    let kids = children(node);
    replace_with_many(node, &kids);
}

/// Removes all children of `node`.
pub fn clear_children(node: &Handle) {
    for c in children(node) {
        remove(&c);
    }
}

/// Replaces all children of `node` with one text node.
pub fn set_text(node: &Handle, text: &str) {
    clear_children(node);
    append_child(node, &create_text(text));
}

// ---------------------------------------------------------------------------
// Serialization
// ---------------------------------------------------------------------------

/// Serializes `node` itself (outer XHTML). For a document node this
/// serializes its children and declares the XHTML namespace on the root
/// element; otherwise the context is assumed to be XHTML already (the chapter
/// wrapper declares it), so only SVG/MathML roots get an `xmlns`.
pub fn serialize_xhtml(node: &Handle) -> String {
    let mut out = String::new();
    match node.data {
        NodeData::Document => {
            for c in node.children.borrow().iter() {
                write_node(c, "", &mut out);
            }
        }
        _ => write_node(node, XHTML_NS, &mut out),
    }
    out
}

/// Serializes only the children of `node` (inner XHTML), e.g. the content
/// of `<body>` for `Chapter::body_xhtml`.
pub fn serialize_inner_xhtml(node: &Handle) -> String {
    let mut out = String::new();
    let ctx_ns = match &node.data {
        NodeData::Element { name, .. } => name.ns.to_string(),
        NodeData::Document => String::new(),
        _ => XHTML_NS.to_string(),
    };
    for c in node.children.borrow().iter() {
        write_node(c, &ctx_ns, &mut out);
    }
    out
}

fn write_node(node: &Handle, parent_ns: &str, out: &mut String) {
    match &node.data {
        NodeData::Text { contents } => escape_into(&contents.borrow(), false, out),
        NodeData::Element { name, attrs, .. } => {
            write_element(node, name, &attrs.borrow(), parent_ns, out)
        }
        NodeData::Document => {
            for c in node.children.borrow().iter() {
                write_node(c, parent_ns, out);
            }
        }
        // Comments, doctypes and processing instructions are dropped.
        _ => {}
    }
}

fn write_element(
    node: &Handle,
    name: &QualName,
    attrs: &[Attribute],
    parent_ns: &str,
    out: &mut String,
) {
    let local: &str = &name.local;
    let ns: &str = &name.ns;
    let ns = if ns.is_empty() { XHTML_NS } else { ns };
    if !is_xml_ncname(local) {
        // Not representable in XML (e.g. Word's `<o:p>`): keep the content.
        for c in node.children.borrow().iter() {
            write_node(c, parent_ns, out);
        }
        return;
    }
    out.push('<');
    out.push_str(local);
    if ns != parent_ns {
        out.push_str(" xmlns=\"");
        escape_into(ns, true, out);
        out.push('"');
    }
    let mut wrote_xlink_decl = false;
    let mut seen: Vec<String> = Vec::new();
    for a in attrs {
        let Some(attr_name) = xml_attr_name(a) else {
            continue;
        };
        if seen.contains(&attr_name) {
            continue;
        }
        if attr_name.starts_with("xlink:") && !wrote_xlink_decl {
            out.push_str(" xmlns:xlink=\"");
            out.push_str(XLINK_NS);
            out.push('"');
            wrote_xlink_decl = true;
        }
        out.push(' ');
        out.push_str(&attr_name);
        out.push_str("=\"");
        escape_into(&a.value, true, out);
        out.push('"');
        seen.push(attr_name);
    }
    let kids = node.children.borrow();
    let is_html = ns == XHTML_NS;
    if is_html && VOID_ELEMENTS.contains(&local.to_ascii_lowercase().as_str()) {
        out.push_str("/>");
        return;
    }
    if kids.is_empty() && !is_html {
        out.push_str("/>");
        return;
    }
    out.push('>');
    for c in kids.iter() {
        write_node(c, ns, out);
    }
    out.push_str("</");
    out.push_str(local);
    out.push('>');
}

/// XML attribute name for an html5ever attribute, or `None` if it cannot be
/// written as well-formed, namespace-valid XML.
fn xml_attr_name(a: &Attribute) -> Option<String> {
    let local: &str = &a.name.local;
    let ns: &str = &a.name.ns;
    if ns == "http://www.w3.org/2000/xmlns/" || local == "xmlns" {
        // Namespace declarations are emitted by the serializer itself.
        return None;
    }
    if ns == XLINK_NS {
        return is_xml_ncname(local).then(|| format!("xlink:{local}"));
    }
    if ns == XML_NS {
        return is_xml_ncname(local).then(|| format!("xml:{local}"));
    }
    if let Some(rest) = local.strip_prefix("xlink:") {
        return is_xml_ncname(rest).then(|| format!("xlink:{rest}"));
    }
    if let Some(rest) = local.strip_prefix("xml:") {
        return is_xml_ncname(rest).then(|| format!("xml:{rest}"));
    }
    if local.starts_with("xmlns") {
        return None;
    }
    is_xml_ncname(local).then(|| local.to_string())
}

/// Whether `s` is a valid XML NCName (a name without a colon).
fn is_xml_ncname(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    let start_ok = |c: char| c == '_' || c.is_alphabetic();
    let rest_ok =
        |c: char| c == '_' || c == '-' || c == '.' || c.is_alphanumeric() || c == '\u{B7}';
    start_ok(first) && chars.all(rest_ok)
}

fn is_xml_char(c: char) -> bool {
    matches!(c,
        '\u{9}' | '\u{A}' | '\u{D}'
        | '\u{20}'..='\u{D7FF}'
        | '\u{E000}'..='\u{FFFD}'
        | '\u{10000}'..='\u{10FFFF}')
}

/// Escapes text for XML. In attribute mode quotes and newlines are escaped
/// too. Characters that XML 1.0 forbids are dropped.
pub fn escape_into(s: &str, attr: bool, out: &mut String) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attr => out.push_str("&quot;"),
            '\n' if attr => out.push_str("&#10;"),
            '\t' if attr => out.push_str("&#9;"),
            '\r' => out.push_str("&#13;"),
            '\u{A0}' => out.push_str("&#160;"),
            '\u{AD}' => out.push_str("&#173;"),
            c if is_xml_char(c) => out.push(c),
            _ => {}
        }
    }
}

/// Escapes text for use in XML/XHTML content or attribute values.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    escape_into(s, true, &mut out);
    out
}

/// Test helpers shared by all modules' unit tests.
#[cfg(test)]
pub mod tests_support {
    /// Panics unless `body_xhtml` (chapter body content) is well-formed,
    /// namespace-valid XML once wrapped in an XHTML `<body>`.
    pub fn assert_well_formed(body_xhtml: &str) {
        let doc = format!("<body xmlns=\"http://www.w3.org/1999/xhtml\">{body_xhtml}</body>");
        if let Err(e) = roxmltree::Document::parse(&doc) {
            panic!("not well-formed XML ({e}):\n{body_xhtml}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::assert_well_formed;
    use super::*;

    fn body_xhtml(html: &str) -> String {
        let doc = parse(html);
        serialize_inner_xhtml(&body(&doc).unwrap())
    }

    #[test]
    fn void_elements_and_entities() {
        let out = body_xhtml("<p>a&nbsp;b<br>c &amp; d &lt; e<img src=x alt='q\"t'></p>");
        assert_eq!(
            out,
            "<p>a&#160;b<br/>c &amp; d &lt; e<img src=\"x\" alt=\"q&quot;t\"/></p>"
        );
        assert_well_formed(&out);
    }

    #[test]
    fn unclosed_tags_and_bad_attrs() {
        let out = body_xhtml("<div><p>one<p>two<span @click=x data-ok=1>t</div><o:p>w</o:p>");
        assert_well_formed(&out);
        assert!(out.contains("data-ok=\"1\""));
        assert!(!out.contains("@click"));
        assert!(out.contains(">w"));
        assert!(!out.contains("o:p"));
    }

    #[test]
    fn svg_namespace_and_camel_case() {
        let out = body_xhtml(
            r##"<svg viewBox="0 0 10 10" xmlns:xlink="http://www.w3.org/1999/xlink"><use xlink:href="#a"></use><foreignObject><p>x</p></foreignObject></svg>"##,
        );
        assert!(
            out.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 10 10\">"),
            "{out}"
        );
        assert!(out.contains("xmlns:xlink=\"http://www.w3.org/1999/xlink\" xlink:href=\"#a\""));
        assert!(out.contains("<p xmlns=\"http://www.w3.org/1999/xhtml\">x</p>"));
        assert_well_formed(&out);
    }

    #[test]
    fn math_namespace() {
        let out = body_xhtml("<math><mi>x</mi></math>");
        assert_eq!(
            out,
            "<math xmlns=\"http://www.w3.org/1998/Math/MathML\"><mi>x</mi></math>"
        );
    }

    #[test]
    fn style_and_comments() {
        let out = body_xhtml("<p>t</p><style>a > b { x: '&' }</style><!-- c -->");
        assert_eq!(out, "<p>t</p><style>a &gt; b { x: '&amp;' }</style>");
    }

    #[test]
    fn control_chars_dropped() {
        let out = body_xhtml("<p>a\u{1}b</p>");
        assert_eq!(out, "<p>ab</p>");
    }

    #[test]
    fn whole_document_declares_xhtml() {
        let doc = parse("<!doctype html><title>T</title><p>x</p>");
        let out = serialize_xhtml(&doc);
        assert!(out.starts_with("<html xmlns=\"http://www.w3.org/1999/xhtml\">"));
        assert_well_formed(&out);
    }

    #[test]
    fn attributes_and_selection() {
        let doc = parse("<p class='a b'>x <img src=1><IMG src=2 data-res=1></p>");
        let imgs = select_by_tag(&doc, "img");
        assert_eq!(imgs.len(), 2);
        assert_eq!(get_attr(&imgs[1], "data-res").as_deref(), Some("1"));
        remove_attr(&imgs[1], "data-res");
        assert!(!has_attr(&imgs[1], "data-res"));
        set_attr(&imgs[0], "alt", "hi");
        set_attr(&imgs[0], "src", "3");
        assert_eq!(get_attr(&imgs[0], "src").as_deref(), Some("3"));
        let p = first_by_tag(&doc, "p").unwrap();
        assert!(has_class(&p, "b"));
        assert!(!has_class(&p, "c"));
        assert_eq!(text_content(&p), "x ");
        assert_eq!(tag_name(&p).as_deref(), Some("p"));
        assert!(get_attr(&create_text("t"), "x").is_none());
    }

    #[test]
    fn mutation_helpers() {
        let doc = parse("<p><b>old</b> tail</p>");
        let b = first_by_tag(&doc, "b").unwrap();
        let em = create_element("em", &[("class", "n")]);
        append_child(&em, &create_text("new"));
        replace_with(&b, &em);
        let p = first_by_tag(&doc, "p").unwrap();
        assert_eq!(serialize_xhtml(&p), "<p><em class=\"n\">new</em> tail</p>");
        assert!(parent(&b).is_none());

        let frag = parse_fragment("<span>1</span><span>2</span>");
        assert_eq!(frag.len(), 2);
        replace_with_many(&em, &frag);
        assert_eq!(
            serialize_xhtml(&p),
            "<p><span>1</span><span>2</span> tail</p>"
        );
        unwrap(&frag[0]);
        assert_eq!(serialize_xhtml(&p), "<p>1<span>2</span> tail</p>");
        set_text(&p, "<x>");
        assert_eq!(serialize_xhtml(&p), "<p>&lt;x&gt;</p>");
        // Operations on detached nodes are harmless.
        replace_with(&b, &em);
        insert_before(&b, &em);
        remove(&b);
    }

    #[test]
    fn fragment_keeps_style_and_title_in_body_context() {
        let frag = parse_fragment("<style>p{}</style><p>a</p>");
        assert_eq!(frag.len(), 2);
        assert!(is_tag(&frag[0], "style"));
    }

    #[test]
    fn escape_helper() {
        assert_eq!(escape("a<\"&\u{a0}"), "a&lt;&quot;&amp;&#160;");
    }
}
