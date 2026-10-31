//! A small XML reader for XMP sidecars that keeps the byte spans of every start tag, attribute and element, so a
//! merge can splice changes into the original text and leave everything else byte for byte as it was.
//! Strict where it matters: well-formed, one root, no DTD, bound prefixes.

use std::collections::BTreeMap;

pub(crate) const NS_RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";
pub(crate) const NS_XML: &str = "http://www.w3.org/XML/1998/namespace";
pub(crate) const MAX_XMP_BYTES: usize = 2 * 1024 * 1024;

pub(crate) type Scope = BTreeMap<String, String>;

/// Why a document cannot be read (or merged).
#[derive(Debug, Clone)]
pub(crate) struct XmlError(pub String);

impl std::fmt::Display for XmlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

type R<T> = Result<T, XmlError>;

fn err<T>(m: impl Into<String>) -> R<T> {
    Err(XmlError(m.into()))
}

/// An attribute with the byte spans of its text.
#[derive(Debug, Clone)]
pub(crate) struct XAttr {
    pub raw: String,
    pub uri: String,
    pub local: String,
    pub value: String,
    /// Start of the whitespace before the name.
    pub ws: usize,
    /// The value, without its quotes.
    pub vstart: usize,
    pub vend: usize,
    /// After the closing quote.
    pub end: usize,
    pub quote: u8,
}

#[derive(Debug, Clone)]
pub(crate) struct XNode {
    pub name: String,
    pub uri: String,
    pub local: String,
    pub attrs: Vec<XAttr>,
    /// Namespaces declared on this element / in force on it.
    pub decls: Vec<(String, String)>,
    pub scope: Scope,
    pub start: usize,
    pub open_end: usize,
    pub selfclose: bool,
    /// Offset of `</` (the end of the start tag for `<a/>`).
    pub close: usize,
    pub end: usize,
    pub kids: Vec<usize>,
    /// The element's own character data (not its children's).
    pub text: String,
}

pub(crate) struct Tree {
    pub nodes: Vec<XNode>,
    pub root: usize,
}

fn is_name_end(c: u8) -> bool {
    c.is_ascii_whitespace() || matches!(c, b'/' | b'>' | b'=' | b'<' | b'"' | b'\'')
}

fn ns_uri(scope: &Scope, qname: &str, is_element: bool) -> R<String> {
    match qname.split_once(':') {
        None => Ok(if is_element { scope.get("").cloned().unwrap_or_default() } else { String::new() }),
        Some((pfx, _)) => scope.get(pfx).cloned().map_or_else(|| err(format!("unbound prefix {pfx}")), Ok),
    }
}

/// Resolve `&name;` / `&#N;` references and normalise line ends (text) or whitespace (attribute values).
fn decode(raw: &[u8], attr: bool) -> R<String> {
    let s = std::str::from_utf8(raw).or_else(|_| err("not UTF-8"))?;
    let mut out = String::with_capacity(s.len());
    let mut it = s.char_indices().peekable();
    while let Some((i, c)) = it.next() {
        match c {
            '&' => {
                let rest = &s[i + 1..];
                let semi = rest.find(';').map_or_else(|| err("bad entity"), Ok)?;
                let name = &rest[..semi];
                let num = |r: Option<u32>| r.and_then(char::from_u32).map_or_else(|| err("bad character reference"), Ok);
                let ch = match name {
                    "amp" => '&',
                    "lt" => '<',
                    "gt" => '>',
                    "quot" => '"',
                    "apos" => '\'',
                    n if n.starts_with("#x") => num(u32::from_str_radix(&n[2..], 16).ok())?,
                    n if n.starts_with('#') => num(n[1..].parse::<u32>().ok())?,
                    _ => return err(format!("undefined entity {name}")),
                };
                out.push(ch);
                for _ in 0..=semi {
                    it.next();
                }
            }
            '\r' => {
                if it.peek().is_some_and(|(_, n)| *n == '\n') {
                    it.next();
                }
                out.push(if attr { ' ' } else { '\n' });
            }
            '\n' | '\t' if attr => out.push(' '),
            c => out.push(c),
        }
    }
    Ok(out)
}

pub(crate) fn find(data: &[u8], from: usize, pat: &[u8]) -> Option<usize> {
    data.get(from..)?.windows(pat.len()).position(|w| w == pat).map(|p| p + from)
}

struct RawAttr {
    name: String,
    value: String,
    ws: usize,
    vstart: usize,
    vend: usize,
    end: usize,
    quote: u8,
}

/// Parse an XMP document. `strict_utf8`: also refuse a document that declares another encoding (the merge works
/// on UTF-8 text only; plain validation does not mind).
pub(crate) fn parse(data: &[u8], strict_utf8: bool) -> R<Tree> {
    if data.len() >= MAX_XMP_BYTES {
        return err("too large");
    }
    let low = data.to_ascii_lowercase();
    if find(&low, 0, b"<!doctype").is_some() || find(&low, 0, b"<!entity").is_some() {
        return err("DTD/entities not allowed");
    }
    if strict_utf8 {
        std::str::from_utf8(data).or_else(|_| err("not UTF-8"))?;
    }
    let mut i = if data.starts_with(&[0xEF, 0xBB, 0xBF]) { 3 } else { 0 };
    let mut nodes: Vec<XNode> = Vec::new();
    let mut stack: Vec<usize> = Vec::new();
    let mut root: Option<usize> = None;
    let mut seen_content = false;
    while i < data.len() {
        if data[i] != b'<' {
            let end = data[i..].iter().position(|&c| c == b'<').map_or(data.len(), |p| p + i);
            let raw = &data[i..end];
            match stack.last() {
                Some(&top) => {
                    let t = decode(raw, false)?;
                    nodes[top].text.push_str(&t);
                }
                None => {
                    if !raw.iter().all(u8::is_ascii_whitespace) {
                        return err("text outside the root element");
                    }
                }
            }
            i = end;
            continue;
        }
        let rest = &data[i..];
        if rest.starts_with(b"<?") {
            let e = find(data, i + 2, b"?>").map_or_else(|| err("unterminated processing instruction"), Ok)?;
            let body = &data[i + 2..e];
            let is_decl = body.starts_with(b"xml") && body.get(3).is_none_or(|c| c.is_ascii_whitespace());
            if is_decl {
                if seen_content || i > 3 {
                    return err("misplaced XML declaration");
                }
                if strict_utf8 {
                    let t = String::from_utf8_lossy(body).to_ascii_lowercase();
                    if let Some(p) = t.find("encoding") {
                        let v: String = t[p + 8..]
                            .trim_start()
                            .trim_start_matches('=')
                            .trim_start()
                            .chars()
                            .skip(1)
                            .take_while(|c| *c != '"' && *c != '\'')
                            .collect();
                        if v != "utf-8" && v != "utf8" {
                            return err("not UTF-8");
                        }
                    }
                }
            }
            seen_content = true;
            i = e + 2;
        } else if rest.starts_with(b"<!--") {
            let e = find(data, i + 4, b"-->").map_or_else(|| err("unterminated comment"), Ok)?;
            if find(&data[i + 4..e], 0, b"--").is_some() {
                return err("-- inside a comment");
            }
            seen_content = true;
            i = e + 3;
        } else if rest.starts_with(b"<![CDATA[") {
            let e = find(data, i + 9, b"]]>").map_or_else(|| err("unterminated CDATA"), Ok)?;
            let &top = stack.last().map_or_else(|| err("CDATA outside the root element"), Ok)?;
            nodes[top].text.push_str(std::str::from_utf8(&data[i + 9..e]).or_else(|_| err("not UTF-8"))?);
            i = e + 3;
        } else if rest.starts_with(b"<!") {
            return err("unsupported markup declaration");
        } else if rest.starts_with(b"</") {
            let e = find(data, i, b">").map_or_else(|| err("unterminated end tag"), Ok)?;
            let name = std::str::from_utf8(&data[i + 2..e]).or_else(|_| err("not UTF-8"))?.trim_end();
            let top = stack.pop().map_or_else(|| err("unmatched end tag"), Ok)?;
            if nodes[top].name != name {
                return err(format!("mismatched tag {} / {}", nodes[top].name, name));
            }
            nodes[top].close = i;
            nodes[top].end = e + 1;
            i = e + 1;
        } else {
            // a start tag: <name (ws attr="v")* ws? /? >
            if stack.is_empty() && root.is_some() {
                return err("more than one root element");
            }
            let start = i;
            let mut j = i + 1;
            while j < data.len() && !is_name_end(data[j]) {
                j += 1;
            }
            if j == i + 1 {
                return err("bad start tag");
            }
            let name = std::str::from_utf8(&data[i + 1..j]).or_else(|_| err("not UTF-8"))?.to_string();
            let mut raw_attrs: Vec<RawAttr> = Vec::new();
            let selfclose;
            loop {
                let ws = j;
                while j < data.len() && data[j].is_ascii_whitespace() {
                    j += 1;
                }
                match data.get(j) {
                    None => return err("unterminated start tag"),
                    Some(b'>') => {
                        selfclose = false;
                        j += 1;
                        break;
                    }
                    Some(b'/') if data.get(j + 1) == Some(&b'>') => {
                        selfclose = true;
                        j += 2;
                        break;
                    }
                    Some(_) if j == ws => return err("missing whitespace between attributes"),
                    Some(_) => {}
                }
                let ns = j;
                while j < data.len() && !is_name_end(data[j]) {
                    j += 1;
                }
                if j == ns {
                    return err("bad attribute");
                }
                let an = std::str::from_utf8(&data[ns..j]).or_else(|_| err("not UTF-8"))?.to_string();
                while j < data.len() && data[j].is_ascii_whitespace() {
                    j += 1;
                }
                if data.get(j) != Some(&b'=') {
                    return err("attribute without value");
                }
                j += 1;
                while j < data.len() && data[j].is_ascii_whitespace() {
                    j += 1;
                }
                let q = match data.get(j) {
                    Some(&q) if q == b'"' || q == b'\'' => q,
                    _ => return err("unquoted attribute value"),
                };
                let vstart = j + 1;
                let vend = data[vstart..].iter().position(|&c| c == q).map_or_else(|| err("unterminated attribute value"), |p| Ok(p + vstart))?;
                if data[vstart..vend].contains(&b'<') {
                    return err("< in an attribute value");
                }
                let value = decode(&data[vstart..vend], true)?;
                if raw_attrs.iter().any(|a| a.name == an) {
                    return err("duplicate attribute");
                }
                raw_attrs.push(RawAttr { name: an, value, ws, vstart, vend, end: vend + 1, quote: q });
                j = vend + 1;
            }
            let mut scope = match stack.last() {
                Some(&p) => nodes[p].scope.clone(),
                None => BTreeMap::from([("xml".to_string(), NS_XML.to_string())]),
            };
            let mut decls = Vec::new();
            for a in &raw_attrs {
                if a.name == "xmlns" {
                    decls.push((String::new(), a.value.clone()));
                } else if let Some(p) = a.name.strip_prefix("xmlns:") {
                    decls.push((p.to_string(), a.value.clone()));
                }
            }
            for (p, u) in &decls {
                scope.insert(p.clone(), u.clone());
            }
            let mut attrs = Vec::new();
            for a in raw_attrs {
                if a.name == "xmlns" || a.name.starts_with("xmlns:") {
                    continue;
                }
                let uri = ns_uri(&scope, &a.name, false)?;
                let local = a.name.rsplit(':').next().unwrap_or(&a.name).to_string();
                attrs.push(XAttr { raw: a.name, uri, local, value: a.value, ws: a.ws, vstart: a.vstart, vend: a.vend, end: a.end, quote: a.quote });
            }
            let uri = ns_uri(&scope, &name, true)?;
            let local = name.rsplit(':').next().unwrap_or(&name).to_string();
            let idx = nodes.len();
            nodes.push(XNode {
                name,
                uri,
                local,
                attrs,
                decls,
                scope,
                start,
                open_end: j,
                selfclose,
                close: if selfclose { j } else { 0 },
                end: if selfclose { j } else { 0 },
                kids: Vec::new(),
                text: String::new(),
            });
            match stack.last() {
                Some(&p) => nodes[p].kids.push(idx),
                None => root = Some(idx),
            }
            if !selfclose {
                stack.push(idx);
            }
            seen_content = true;
            i = j;
        }
    }
    if !stack.is_empty() {
        return err("unclosed element");
    }
    match root {
        Some(root) => Ok(Tree { nodes, root }),
        None => err("no root element"),
    }
}

impl Tree {
    pub fn node(&self, i: usize) -> &XNode {
        &self.nodes[i]
    }

    /// What an element means, independent of prefixes, attribute order and indentation.
    pub fn canon(&self, i: usize) -> String {
        let n = &self.nodes[i];
        let mut attrs: Vec<(&str, &str, &str)> = n.attrs.iter().map(|a| (a.uri.as_str(), a.local.as_str(), a.value.as_str())).collect();
        attrs.sort();
        let kids: Vec<String> = n.kids.iter().map(|&k| self.canon(k)).collect();
        format!("({:?},{:?},{:?},{:?},[{}])", n.uri, n.local, attrs, n.text.trim(), kids.join(","))
    }
}

/// `validate_xmp`: None if the bytes are an acceptable inbox/sidecar file, else why not.
pub(crate) fn validate(data: &[u8]) -> Option<String> {
    if data.len() >= MAX_XMP_BYTES {
        return Some("too large".into());
    }
    let low = data.to_ascii_lowercase();
    if find(&low, 0, b"<!doctype").is_some() || find(&low, 0, b"<!entity").is_some() {
        return Some("DTD/entities not allowed".into());
    }
    if let Err(e) = parse(data, false) {
        return Some(format!("not well-formed XML ({e})"));
    }
    if find(data, 0, b"lc:settings").is_none() && find(data, 0, b"crs:").is_none() {
        return Some("no lc:settings or crs: develop data".into());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spans_and_scopes() {
        let d = br#"<?xml version="1.0"?><a xmlns="u" xmlns:p="v" p:x='1 &amp; 2'><p:b/>t<c>x</c></a>"#;
        let t = parse(d, true).unwrap();
        let a = t.node(t.root);
        assert_eq!((a.uri.as_str(), a.attrs.len(), a.kids.len()), ("u", 1, 2));
        assert_eq!(a.attrs[0].value, "1 & 2");
        assert_eq!(&d[a.attrs[0].vstart..a.attrs[0].vend], b"1 &amp; 2");
        let b = t.node(a.kids[0]);
        assert!(b.selfclose && b.uri == "v");
        let c = t.node(a.kids[1]);
        assert_eq!(&d[c.start..c.end], b"<c>x</c>");
        assert_eq!(a.text, "t");
    }

    #[test]
    fn rejects_what_is_not_xml() {
        for bad in [
            "",
            "<a>",
            "<a></b>",
            "<a/><b/>",
            "x<a/>",
            "<a/>x",
            "<a p:x='1'/>",
            "<a x='1' x='2'/>",
            "<a x=1/>",
            "<!DOCTYPE a><a/>",
            "<a>&e;</a>",
            "<a><!-- -- --></a>",
        ] {
            assert!(parse(bad.as_bytes(), true).is_err(), "{bad}");
        }
        assert!(validate(b"<a>crs:</a>").is_none());
        assert!(validate(b"<a>nothing</a>").is_some());
    }
}
