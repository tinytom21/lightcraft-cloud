//! Three-way merge of two edits of one XMP sidecar, property by property (FORMAT.md, "Conflict policy" 3).
//!
//! The result is the PC's document with the changed properties spliced into its original bytes: foreign
//! properties, namespace prefixes, the xpacket wrapper and the layout stay as they were.

use std::collections::HashMap;

use super::json::{self, J, Layout};
use super::xmp::{self, NS_RDF, NS_XML, Scope, Tree, XmlError};

const NS_XMP: &str = "http://ns.adobe.com/xap/1.0/";
const NS_LC: &str = "http://ns.lightcraft.app/lc/1.0/";
/// `lc:settings` sections merged as a whole.
const ATOMIC_SECTIONS: [&str; 5] = ["crop", "geometry", "curve", "profile", "wb"];

type Key = (String, String);

fn settings_key() -> Key {
    (NS_LC.to_string(), "settings".to_string())
}

fn housekeeping(k: &Key) -> bool {
    k.0 == NS_XMP && matches!(k.1.as_str(), "MetadataDate" | "ModifyDate" | "CreatorTool")
}

/// These documents cannot be merged; the caller falls back to newest-wins.
#[derive(Debug)]
struct MergeFail(String);

impl From<XmlError> for MergeFail {
    fn from(e: XmlError) -> Self {
        MergeFail(e.0)
    }
}

type R<T> = Result<T, MergeFail>;

fn fail<T>(m: impl Into<String>) -> R<T> {
    Err(MergeFail(m.into()))
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Val {
    /// A plain text value (trimmed): the same as an attribute or as an element.
    T(String),
    /// A complex value, by meaning.
    X(String),
}

/// One `rdf:Description` property, written as an attribute or as a child element.
struct Prop {
    desc: usize,
    node: Option<usize>,
    /// Index into the description's attributes.
    attr: Option<usize>,
    name: String,
    text: Option<String>,
    value: Val,
}

struct XDoc<'a> {
    data: &'a [u8],
    tree: Tree,
    descs: Vec<usize>,
    props: Vec<(Key, Prop)>,
    index: HashMap<Key, usize>,
}

impl<'a> XDoc<'a> {
    fn new(data: &'a [u8]) -> R<Self> {
        let tree = xmp::parse(data, true)?;
        let mut descs = Vec::new();
        let mut todo = vec![tree.root];
        while let Some(n) = todo.pop() {
            let node = tree.node(n);
            if node.uri == NS_RDF && node.local == "Description" {
                descs.push(n);
            } else {
                todo.extend(node.kids.iter().rev());
            }
        }
        let mut doc = XDoc { data, tree, descs: descs.clone(), props: Vec::new(), index: HashMap::new() };
        for d in descs {
            let node = doc.tree.node(d).clone();
            let mut found = Vec::new();
            for (ai, a) in node.attrs.iter().enumerate() {
                if a.uri == NS_RDF || a.uri == NS_XML {
                    continue;
                }
                found.push((
                    (a.uri.clone(), a.local.clone()),
                    Prop {
                        desc: d,
                        node: None,
                        attr: Some(ai),
                        name: a.raw.clone(),
                        text: Some(a.value.clone()),
                        value: Val::T(a.value.trim().to_string()),
                    },
                ));
            }
            for &k in &node.kids {
                let kn = doc.tree.node(k);
                let text = (kn.kids.is_empty() && kn.attrs.is_empty()).then(|| kn.text.clone());
                let value = match &text {
                    Some(t) => Val::T(t.trim().to_string()),
                    None => Val::X(doc.tree.canon(k)),
                };
                found.push(((kn.uri.clone(), kn.local.clone()), Prop { desc: d, node: Some(k), attr: None, name: kn.name.clone(), text, value }));
            }
            for (key, pr) in found {
                if doc.index.contains_key(&key) {
                    return fail(format!("property {} appears twice", pr.name));
                }
                doc.index.insert(key.clone(), doc.props.len());
                doc.props.push((key, pr));
            }
        }
        Ok(doc)
    }

    fn get(&self, k: &Key) -> Option<&Prop> {
        self.index.get(k).map(|&i| &self.props[i].1)
    }
}

fn val(p: Option<&Prop>) -> Option<&Val> {
    p.map(|p| &p.value)
}

// ------------------------------------------------------------------ JSON

fn same_json(a: Option<&J>, b: Option<&J>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(a), Some(b)) => a.canon() == b.canon(),
        _ => false,
    }
}

/// Three-way merge of `lc:settings` values (PC `p`, phone `m`, common ancestor `b`). Objects merge key by key, except the
/// atomic sections; everything else (numbers, strings, lists) is a single value.
fn merge_json(b: Option<&J>, p: Option<&J>, m: Option<&J>, path: &mut Vec<String>, phone_newer: bool, conflicts: &mut Vec<Vec<String>>) -> Option<J> {
    if same_json(p, m) || same_json(m, b) {
        return p.cloned();
    }
    if same_json(p, b) {
        return m.cloned();
    }
    if let (Some(J::Obj(po)), Some(J::Obj(mo))) = (p, m) {
        let b_is_ok = matches!(b, None | Some(J::Obj(_)));
        if b_is_ok && !(path.len() == 1 && ATOMIC_SECTIONS.contains(&path[0].as_str())) {
            let mut out: Vec<(String, J)> = Vec::new();
            let keys: Vec<&String> =
                po.iter().map(|(k, _)| k).chain(mo.iter().map(|(k, _)| k).filter(|k| !po.iter().any(|(pk, _)| pk == *k))).collect();
            for k in keys {
                path.push(k.clone());
                let v = merge_json(b.and_then(|b| b.get(k)), p.and_then(|p| p.get(k)), m.and_then(|m| m.get(k)), path, phone_newer, conflicts);
                path.pop();
                if let Some(v) = v {
                    out.push((k.clone(), v));
                }
            }
            return Some(J::Obj(out));
        }
    }
    conflicts.push(path.clone());
    if phone_newer { m.cloned() } else { p.cloned() }
}

fn settings_of(doc: &XDoc) -> R<Option<J>> {
    let Some(pr) = doc.get(&settings_key()) else { return Ok(None) };
    let obj = match &pr.text {
        Some(t) => json::parse(t).map_err(MergeFail)?,
        None => return fail("lc:settings is not a JSON object"),
    };
    if !obj.is_obj() {
        return fail("lc:settings is not a JSON object");
    }
    Ok(Some(obj))
}

// ------------------------------------------------------------------ splicing

fn esc_text(s: &str) -> Vec<u8> {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('\r', "&#13;").into_bytes()
}

fn esc_attr(s: &str, quote: u8) -> Vec<u8> {
    let s = s.replace('&', "&amp;").replace('<', "&lt;").replace('\t', "&#9;").replace('\n', "&#10;").replace('\r', "&#13;");
    if quote == b'"' { s.replace('"', "&quot;") } else { s.replace('\'', "&apos;") }.into_bytes()
}

fn xmlns_decl(pfx: &str, uri: &str) -> Vec<u8> {
    let mut v = if pfx.is_empty() { b" xmlns=\"".to_vec() } else { format!(" xmlns:{pfx}=\"").into_bytes() };
    v.extend(esc_attr(uri, b'"'));
    v.push(b'"');
    v
}

fn used_prefixes(tree: &Tree, n: usize, out: &mut std::collections::BTreeSet<String>) {
    let node = tree.node(n);
    out.insert(node.name.rsplit_once(':').map(|(p, _)| p.to_string()).unwrap_or_default());
    for a in &node.attrs {
        if !a.uri.is_empty() && a.uri != NS_XML {
            out.insert(a.raw.rsplit_once(':').map(|(p, _)| p.to_string()).unwrap_or_default());
        }
    }
    for &k in &node.kids {
        used_prefixes(tree, k, out);
    }
}

/// Edits to the PC document, spliced into its original bytes.
struct Splice<'a> {
    doc: &'a XDoc<'a>,
    /// (start, end, order, text)
    ops: Vec<(usize, usize, usize, Vec<u8>)>,
    /// Prefixes declared on the first Description by an insertion.
    added: Scope,
    /// The first Description was `<.../>` and has been opened up for new child elements.
    expanded: bool,
}

impl<'a> Splice<'a> {
    fn new(doc: &'a XDoc<'a>) -> Self {
        Splice { doc, ops: Vec::new(), added: Scope::new(), expanded: false }
    }

    /// Replace data[start..end] (an insertion when start == end). Edits at the same place keep their order.
    fn put(&mut self, start: usize, end: usize, text: Vec<u8>, last: bool) {
        let order = if last { 1 << 30 } else { self.ops.len() };
        self.ops.push((start, end, order, text));
    }

    fn result(mut self) -> R<Vec<u8>> {
        self.ops.sort_by_key(|(s, e, o, _)| (*s, *e, *o));
        let mut out = Vec::new();
        let mut pos = 0;
        for (start, end, _, text) in &self.ops {
            if *start < pos {
                return fail("overlapping edits");
            }
            out.extend_from_slice(&self.doc.data[pos..*start]);
            out.extend_from_slice(text);
            pos = *end;
        }
        out.extend_from_slice(&self.doc.data[pos..]);
        Ok(out)
    }

    fn scope(&self) -> Scope {
        let mut s = self.doc.tree.node(self.doc.descs[0]).scope.clone();
        s.extend(self.added.clone());
        s
    }

    fn set_text(&mut self, pp: &Prop, text: &str) {
        let tree = &self.doc.tree;
        if let Some(ai) = pp.attr {
            let a = &tree.node(pp.desc).attrs[ai];
            let (vs, ve, q) = (a.vstart, a.vend, a.quote);
            self.put(vs, ve, esc_attr(text, q), false);
        } else if let Some(ni) = pp.node {
            let n = tree.node(ni);
            if n.selfclose {
                let mut t = b">".to_vec();
                t.extend(esc_text(text));
                t.extend(format!("</{}>", n.name).bytes());
                let (a, b) = (n.open_end - 2, n.open_end);
                self.put(a, b, t, false);
            } else {
                let (a, b) = (n.open_end, n.close);
                self.put(a, b, esc_text(text), false);
            }
        }
    }

    fn remove(&mut self, pp: &Prop) {
        let tree = &self.doc.tree;
        if let Some(ai) = pp.attr {
            let a = &tree.node(pp.desc).attrs[ai];
            let (ws, end) = (a.ws, a.end);
            self.put(ws, end, Vec::new(), false);
            return;
        }
        let Some(ni) = pp.node else { return };
        let n = tree.node(ni);
        let data = self.doc.data;
        let mut s = n.start;
        while s > 0 && (data[s - 1] == b' ' || data[s - 1] == b'\t') {
            s -= 1;
        }
        if s > 0 && data[s - 1] == b'\n' {
            s -= if s >= 2 && data[s - 2] == b'\r' { 2 } else { 1 };
        }
        let end = n.end;
        self.put(s, end, Vec::new(), false);
    }

    /// The phone's property as element text valid where the PC's `scope` applies: prefixes the PC binds differently
    /// (or not at all) are declared on the element itself.
    fn element(&self, mp: &Prop, mdoc: &XDoc, key: &Key, scope: &Scope) -> R<Vec<u8>> {
        if let Some(ai) = mp.attr {
            let a = &mdoc.tree.node(mp.desc).attrs[ai];
            let Some((pfx, _)) = a.raw.split_once(':') else {
                return fail("cannot write a namespace-less attribute as an element");
            };
            let decl = if scope.get(pfx) == Some(&key.0) { Vec::new() } else { xmlns_decl(pfx, &key.0) };
            let mut out = format!("<{}", a.raw).into_bytes();
            out.extend(decl);
            out.push(b'>');
            out.extend(esc_text(&a.value));
            out.extend(format!("</{}>", a.raw).bytes());
            return Ok(out);
        }
        let ni = mp.node.unwrap_or(0);
        let n = mdoc.tree.node(ni);
        let mut decl = Vec::new();
        let mut used = std::collections::BTreeSet::new();
        used_prefixes(&mdoc.tree, ni, &mut used);
        for pfx in used {
            let want = n.scope.get(&pfx).cloned().or_else(|| pfx.is_empty().then(String::new));
            if let Some(want) = want
                && !n.decls.iter().any(|(p, _)| *p == pfx)
                && scope.get(&pfx).map_or("", String::as_str) != want
            {
                decl.extend(xmlns_decl(&pfx, &want));
            }
        }
        let k = n.name.len() + 1;
        let mut out = mdoc.data[n.start..n.start + k].to_vec();
        out.extend(decl);
        out.extend_from_slice(&mdoc.data[n.start + k..n.end]);
        Ok(out)
    }

    /// Add the phone's property to the first Description of the PC document.
    fn add(&mut self, mp: &Prop, mdoc: &XDoc, key: &Key) -> R<()> {
        let tree = &self.doc.tree;
        let d = tree.node(self.doc.descs[0]);
        let data = self.doc.data;
        if let Some(ai) = mp.attr {
            let a = &mdoc.tree.node(mp.desc).attrs[ai];
            let scope = self.scope();
            let uri = &key.0;
            let pfx = a.raw.split_once(':').map_or("", |(p, _)| p).to_string();
            let at = d.open_end - if d.selfclose { 2 } else { 1 };
            let mut name = a.raw.clone();
            if !uri.is_empty() && scope.get(&pfx) != Some(uri) {
                let have = scope.iter().find(|(p, u)| *u == uri && !p.is_empty()).map(|(p, _)| p.clone());
                if let Some(h) = have {
                    name = format!("{h}:{}", a.local);
                } else if !scope.contains_key(&pfx) {
                    self.added.insert(pfx.clone(), uri.clone());
                    self.put(at, at, xmlns_decl(&pfx, uri), false);
                } else {
                    return fail(format!("prefix {pfx} is bound differently in the PC sidecar"));
                }
            }
            let mut t = format!(" {name}=\"").into_bytes();
            t.extend(esc_attr(&a.value, b'"'));
            t.push(b'"');
            self.put(at, at, t, false);
            return Ok(());
        }
        let raw = self.element(mp, mdoc, key, &self.scope())?;
        if let Some(&last) = d.kids.last() {
            // after the last property, indented like it
            let ln = tree.node(last);
            let mut i = ln.start;
            while i > 0 && matches!(data[i - 1], b' ' | b'\t' | b'\r' | b'\n') {
                i -= 1;
            }
            let gap = &data[i..ln.start];
            let mut t = if gap.contains(&b'\n') { gap.to_vec() } else { Vec::new() };
            t.extend(raw);
            let end = ln.end;
            self.put(end, end, t, false);
            return Ok(());
        }
        let mut i = d.start;
        while i > 0 && matches!(data[i - 1], b' ' | b'\t') {
            i -= 1;
        }
        let indent = (i > 0 && data[i - 1] == b'\n').then(|| &data[i..d.start]);
        let (outer, inner) = match indent {
            None => (Vec::new(), Vec::new()),
            Some(ind) => {
                let mut o = b"\n".to_vec();
                o.extend(ind);
                let mut n = o.clone();
                n.extend(b"  ");
                (o, n)
            }
        };
        if d.selfclose && !self.expanded {
            self.expanded = true;
            self.put(d.open_end - 2, d.open_end, b">".to_vec(), false);
            let mut t = outer;
            t.extend(format!("</{}>", d.name).bytes());
            self.put(d.open_end, d.open_end, t, true);
        }
        let mut t = inner;
        t.extend(raw);
        self.put(d.open_end, d.open_end, t, false);
        Ok(())
    }

    /// Make the PC's property `pp` (None: absent) read like the phone's `mp` (None: absent).
    fn take(&mut self, pp: Option<&Prop>, mp: Option<&Prop>, mdoc: &XDoc, key: &Key) -> R<()> {
        match (pp, mp) {
            (Some(pp), None) => self.remove(pp),
            (None, Some(mp)) => self.add(mp, mdoc, key)?,
            (Some(pp), Some(mp)) => {
                if let (Some(_), Some(mt)) = (&pp.text, &mp.text) {
                    self.set_text(pp, mt);
                } else if let Some(ni) = pp.node {
                    let node = self.doc.tree.node(pp.desc).scope.clone();
                    let el = self.element(mp, mdoc, key, &node)?;
                    let (s, e) = (self.doc.tree.node(ni).start, self.doc.tree.node(ni).end);
                    self.put(s, e, el, false);
                } else {
                    self.remove(pp);
                    self.add(mp, mdoc, key)?;
                }
            }
            (None, None) => {}
        }
        Ok(())
    }
}

/// Which side's value a property takes: `Some(true)` the PC's, `Some(false)` the phone's, `None` a conflict.
fn three_way(b: Option<&Val>, p: Option<&Val>, m: Option<&Val>) -> Option<bool> {
    if p == m || m == b {
        Some(true)
    } else if p == b {
        Some(false)
    } else {
        None
    }
}

enum Plan {
    Pc,
    Phone,
    Json(J),
}

/// What a property means, for the final self-check (`lc:settings` by its JSON, not its layout).
#[derive(PartialEq, Debug)]
enum Cmp {
    Json(String),
    Bad(Val),
    V(Val),
}

fn prop_cmp(key: &Key, pr: &Prop) -> Cmp {
    if *key == settings_key() {
        return match pr.text.as_deref().map(json::parse) {
            Some(Ok(j)) => Cmp::Json(j.canon()),
            _ => Cmp::Bad(pr.value.clone()),
        };
    }
    Cmp::V(pr.value.clone())
}

fn merge_inner(base: &[u8], pc: &[u8], phone: &[u8], phone_newer: bool) -> R<(Vec<u8>, Vec<String>)> {
    let (b, p, m) = (XDoc::new(base)?, XDoc::new(pc)?, XDoc::new(phone)?);
    if b.descs.is_empty() || p.descs.is_empty() || m.descs.is_empty() {
        return fail("no rdf:Description");
    }
    let (sb, sp, sm) = (settings_of(&b)?, settings_of(&p)?, settings_of(&m)?);
    if sb.is_none() && (sp.is_some() || sm.is_some()) {
        return fail("the base has no lc:settings");
    }
    if let (Some(sp), Some(sm)) = (&sp, &sm) {
        for k in ["version", "process"] {
            if sp.get(k).map(J::canon) != sm.get(k).map(J::canon) {
                return fail("lc:settings version or process differ");
            }
        }
    }
    let mut conflicts: Vec<String> = Vec::new();
    let mut plan: Vec<(Key, Plan)> = Vec::new();
    let mut keys: Vec<Key> = Vec::new();
    for (k, _) in p.props.iter().chain(m.props.iter()).chain(b.props.iter()) {
        if !keys.contains(k) {
            keys.push(k.clone());
        }
    }
    for key in keys {
        let (bp, pp, mp) = (b.get(&key), p.get(&key), m.get(&key));
        if housekeeping(&key) {
            // the newer side's value, when it has one; never a conflict
            plan.push((key, if mp.is_some() && (phone_newer || pp.is_none()) { Plan::Phone } else { Plan::Pc }));
        } else if key == settings_key() && sb.is_some() && sp.is_some() && sm.is_some() {
            let mut leaves = Vec::new();
            let merged = merge_json(sb.as_ref(), sp.as_ref(), sm.as_ref(), &mut Vec::new(), phone_newer, &mut leaves).unwrap_or(J::Null);
            let name = pp.map(|p| p.name.clone()).unwrap_or_default();
            for path in leaves {
                conflicts.push(format!("{name}{}", path.iter().map(|k| format!("/{k}")).collect::<String>()));
            }
            plan.push((key, Plan::Json(merged)));
        } else {
            let pick = match three_way(val(bp), val(pp), val(mp)) {
                Some(pc_side) => pc_side,
                None => {
                    conflicts.push(pp.or(mp).or(bp).map(|p| p.name.clone()).unwrap_or_default());
                    !phone_newer
                }
            };
            plan.push((key, if pick { Plan::Pc } else { Plan::Phone }));
        }
    }
    let mut edit = Splice::new(&p);
    for (key, what) in &plan {
        let (pp, mp) = (p.get(key), m.get(key));
        match what {
            Plan::Json(obj) => {
                if !same_json(Some(obj), sp.as_ref()) {
                    let Some(pp) = pp else { return fail("lc:settings vanished") };
                    let raw = pp.text.clone().unwrap_or_default();
                    let layout = Layout::of(&raw, pp.attr.is_some());
                    let lead = raw.len() - raw.trim_start().len();
                    let trail = raw.len() - raw.trim_end().len();
                    let text = format!("{}{}{}", &raw[..lead], json::dump(obj, &layout), &raw[raw.len() - trail..]);
                    edit.set_text(pp, &text);
                }
            }
            Plan::Phone => {
                if val(pp) != val(mp) {
                    edit.take(pp, mp, &m, key)?;
                }
            }
            Plan::Pc => {}
        }
    }
    let out = edit.result()?;
    // self-check: the result must mean exactly what the plan says (a bug here falls back to newest-wins)
    let mut want: HashMap<Key, Cmp> = HashMap::new();
    for (key, what) in &plan {
        let c = match what {
            Plan::Json(j) => Some(Cmp::Json(j.canon())),
            Plan::Pc => p.get(key).map(|pr| prop_cmp(key, pr)),
            Plan::Phone => m.get(key).map(|pr| prop_cmp(key, pr)),
        };
        if let Some(c) = c {
            want.insert(key.clone(), c);
        }
    }
    let got_doc = XDoc::new(&out)?;
    let got: HashMap<Key, Cmp> = got_doc.props.iter().map(|(k, pr)| (k.clone(), prop_cmp(k, pr))).collect();
    if got != want {
        return fail("self-check: the result differs from the plan");
    }
    if let Some(e) = xmp::validate(&out) {
        return fail(e);
    }
    Ok((out, conflicts))
}

/// Three-way merge, property by property, of two edits of the sidecar `base`: the PC's (`pc`) and the phone's
/// (`phone`). A property only one side changed takes that side's value; one both changed takes the newer side's and is
/// reported. Returns the merged bytes and the conflicts, or `None` when the documents cannot be merged safely (the
/// caller then falls back to newest-wins). The result is the PC document with edits spliced in.
pub(crate) fn merge_sidecars(base: &[u8], pc: &[u8], phone: &[u8], phone_newer: bool) -> Option<(Vec<u8>, Vec<String>)> {
    merge_inner(base, pc, phone, phone_newer).map_err(|e| log::debug!("sidecar merge gave up: {}", e.0)).ok()
}
