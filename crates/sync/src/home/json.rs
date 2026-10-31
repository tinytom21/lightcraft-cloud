//! A JSON value that keeps the order of an object's keys and the text of its numbers, so a merged
//! `lc:settings` keeps the layout of the PC's document (the Python publisher relied on dict order).

use std::fmt::Write;

#[derive(Debug, Clone)]
pub(crate) enum J {
    Null,
    Bool(bool),
    /// The number as written.
    Num(String),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

impl J {
    pub fn get(&self, k: &str) -> Option<&J> {
        match self {
            J::Obj(v) => v.iter().find(|(key, _)| key == k).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn is_obj(&self) -> bool {
        matches!(self, J::Obj(_))
    }

    /// Meaning, independent of key order and number spelling (`0.60` = `0.6`, but `1` is not `1.0`).
    pub fn canon(&self) -> String {
        let mut s = String::new();
        self.canon_into(&mut s);
        s
    }

    fn canon_into(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => out.push_str(&canon_num(n)),
            J::Str(s) => quote(s, out),
            J::Arr(v) => {
                out.push('[');
                for (i, x) in v.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    x.canon_into(out);
                }
                out.push(']');
            }
            J::Obj(v) => {
                let mut sorted: Vec<&(String, J)> = v.iter().collect();
                sorted.sort_by(|a, b| a.0.cmp(&b.0));
                out.push('{');
                for (i, (k, x)) in sorted.into_iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    quote(k, out);
                    out.push(':');
                    x.canon_into(out);
                }
                out.push('}');
            }
        }
    }
}

fn canon_num(n: &str) -> String {
    let float = n.contains(['.', 'e', 'E']);
    if float {
        match n.parse::<f64>() {
            Ok(f) => format!("f{f:?}"),
            Err(_) => format!("f{n}"),
        }
    } else {
        let t = n.trim_start_matches('-');
        let t = t.trim_start_matches('0');
        let neg = n.starts_with('-') && !t.is_empty();
        format!("i{}{}", if neg { "-" } else { "" }, if t.is_empty() { "0" } else { t })
    }
}

pub(crate) fn quote(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

// ------------------------------------------------------------------ parse

struct P<'a> {
    b: &'a [u8],
    i: usize,
    depth: usize,
}

pub(crate) fn parse(text: &str) -> Result<J, String> {
    let mut p = P { b: text.as_bytes(), i: 0, depth: 0 };
    p.ws();
    let v = p.value()?;
    p.ws();
    if p.i != p.b.len() {
        return Err(format!("extra data at {}", p.i));
    }
    Ok(v)
}

impl P<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn eat(&mut self, lit: &str) -> Result<(), String> {
        if self.b[self.i..].starts_with(lit.as_bytes()) {
            self.i += lit.len();
            Ok(())
        } else {
            Err(format!("bad JSON at {}", self.i))
        }
    }

    fn value(&mut self) -> Result<J, String> {
        self.depth += 1;
        if self.depth > 200 {
            return Err("JSON nested too deep".into());
        }
        let r = self.value_inner();
        self.depth -= 1;
        r
    }

    fn value_inner(&mut self) -> Result<J, String> {
        match self.b.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let mut v: Vec<(String, J)> = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b'}') {
                    self.i += 1;
                    return Ok(J::Obj(v));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    self.eat(":")?;
                    self.ws();
                    let x = self.value()?;
                    // (a repeated key: the last value wins, in the first one's place, like a Python dict)
                    match v.iter_mut().find(|(key, _)| *key == k) {
                        Some(slot) => slot.1 = x,
                        None => v.push((k, x)),
                    }
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(J::Obj(v));
                        }
                        _ => return Err(format!("bad JSON object at {}", self.i)),
                    }
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut v = Vec::new();
                self.ws();
                if self.b.get(self.i) == Some(&b']') {
                    self.i += 1;
                    return Ok(J::Arr(v));
                }
                loop {
                    self.ws();
                    v.push(self.value()?);
                    self.ws();
                    match self.b.get(self.i) {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(J::Arr(v));
                        }
                        _ => return Err(format!("bad JSON array at {}", self.i)),
                    }
                }
            }
            Some(b'"') => Ok(J::Str(self.string()?)),
            Some(b't') => self.eat("true").map(|_| J::Bool(true)),
            Some(b'f') => self.eat("false").map(|_| J::Bool(false)),
            Some(b'n') => self.eat("null").map(|_| J::Null),
            Some(c) if c.is_ascii_digit() || *c == b'-' => {
                let s = self.i;
                self.i += 1;
                while self.i < self.b.len() && matches!(self.b[self.i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                    self.i += 1;
                }
                let t = std::str::from_utf8(&self.b[s..self.i]).map_err(|e| e.to_string())?;
                t.parse::<f64>().map_err(|_| format!("bad number {t}"))?;
                Ok(J::Num(t.to_string()))
            }
            _ => Err(format!("bad JSON at {}", self.i)),
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        let h = self.b.get(self.i..self.i + 4).and_then(|h| std::str::from_utf8(h).ok()).ok_or("bad \\u escape")?;
        self.i += 4;
        u32::from_str_radix(h, 16).map_err(|_| "bad \\u escape".to_string())
    }

    fn string(&mut self) -> Result<String, String> {
        if self.b.get(self.i) != Some(&b'"') {
            return Err(format!("expected a string at {}", self.i));
        }
        self.i += 1;
        let mut out = Vec::new();
        loop {
            let c = *self.b.get(self.i).ok_or("unterminated string")?;
            self.i += 1;
            match c {
                b'"' => break,
                b'\\' => {
                    let e = *self.b.get(self.i).ok_or("unterminated string")?;
                    self.i += 1;
                    let ch = match e {
                        b'"' => '"',
                        b'\\' => '\\',
                        b'/' => '/',
                        b'b' => '\u{8}',
                        b'f' => '\u{c}',
                        b'n' => '\n',
                        b'r' => '\r',
                        b't' => '\t',
                        b'u' => {
                            let mut cp = self.hex4()?;
                            if (0xD800..0xDC00).contains(&cp) && self.b[self.i..].starts_with(b"\\u") {
                                let save = self.i;
                                self.i += 2;
                                let lo = self.hex4()?;
                                if (0xDC00..0xE000).contains(&lo) {
                                    cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                                } else {
                                    self.i = save;
                                }
                            }
                            char::from_u32(cp).unwrap_or('\u{fffd}')
                        }
                        _ => return Err("bad escape".into()),
                    };
                    let mut buf = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                }
                c if c < 0x20 => return Err("control character in string".into()),
                c => out.push(c),
            }
        }
        String::from_utf8(out).map_err(|e| e.to_string())
    }
}

// ------------------------------------------------------------------ write

/// How a document's JSON is laid out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Layout {
    /// `{"a":1}`
    Compact,
    /// `{"a": 1, "b": 2}`
    Spaced,
    /// One item per line; the string is one level of indent.
    Indent(String),
}

impl Layout {
    /// The layout of `raw` (the text of an existing JSON value): indented when it spans lines (and is an element),
    /// spaced when its first key is followed by `: `, else compact.
    pub fn of(raw: &str, attr: bool) -> Layout {
        let s = raw.trim();
        if s.contains('\n') && !attr {
            let ind = s.match_indices('\n').find_map(|(i, _)| {
                let rest = &s[i + 1..];
                let n = rest.len() - rest.trim_start_matches([' ', '\t']).len();
                (n > 0 && !rest[n..].is_empty() && !rest[n..].starts_with(char::is_whitespace)).then(|| rest[..n].to_string())
            });
            return Layout::Indent(match ind {
                Some(i) if i.starts_with('\t') => "\t".to_string(),
                Some(i) => " ".repeat(i.len()),
                None => "  ".to_string(),
            });
        }
        // `{ws"key"ws:ws`
        let b = s.as_bytes();
        let spaced = (|| {
            let mut i = 0;
            if b.first() != Some(&b'{') {
                return false;
            }
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            if b.get(i) != Some(&b'"') {
                return false;
            }
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
            while i < b.len() && b[i].is_ascii_whitespace() {
                i += 1;
            }
            b.get(i) == Some(&b':') && b.get(i + 1).is_some_and(u8::is_ascii_whitespace)
        })();
        if spaced { Layout::Spaced } else { Layout::Compact }
    }
}

pub(crate) fn dump(v: &J, layout: &Layout) -> String {
    let mut s = String::new();
    dump_into(v, layout, 0, &mut s);
    s
}

fn dump_into(v: &J, layout: &Layout, level: usize, out: &mut String) {
    match v {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Num(n) => out.push_str(n),
        J::Str(s) => quote(s, out),
        J::Arr(items) if items.is_empty() => out.push_str("[]"),
        J::Obj(items) if items.is_empty() => out.push_str("{}"),
        J::Arr(items) => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                sep(layout, i, level + 1, out);
                dump_into(x, layout, level + 1, out);
            }
            close(layout, level, out);
            out.push(']');
        }
        J::Obj(items) => {
            out.push('{');
            for (i, (k, x)) in items.iter().enumerate() {
                sep(layout, i, level + 1, out);
                quote(k, out);
                out.push_str(if *layout == Layout::Compact { ":" } else { ": " });
                dump_into(x, layout, level + 1, out);
            }
            close(layout, level, out);
            out.push('}');
        }
    }
}

fn sep(layout: &Layout, i: usize, level: usize, out: &mut String) {
    match layout {
        Layout::Compact if i > 0 => out.push(','),
        Layout::Spaced if i > 0 => out.push_str(", "),
        Layout::Indent(ind) => {
            if i > 0 {
                out.push(',');
            }
            out.push('\n');
            out.push_str(&ind.repeat(level));
        }
        _ => {}
    }
}

fn close(layout: &Layout, level: usize, out: &mut String) {
    if let Layout::Indent(ind) = layout {
        out.push('\n');
        out.push_str(&ind.repeat(level));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_keep_order_and_numbers() {
        let t = r#"{"b":1,"a":[0.60,{"z":null,"y":"é\n"}],"c":1.0}"#;
        let v = parse(t).unwrap();
        assert_eq!(dump(&v, &Layout::Compact), r#"{"b":1,"a":[0.60,{"z":null,"y":"é\n"}],"c":1.0}"#);
        assert_eq!(parse("{\"x\":0.6}").unwrap().canon(), parse("{\"x\":0.60}").unwrap().canon());
        assert_ne!(parse("1").unwrap().canon(), parse("1.0").unwrap().canon());
        assert_eq!(parse("{\"a\":1,\"b\":2}").unwrap().canon(), parse("{\"b\":2,\"a\":1}").unwrap().canon());
        assert!(parse("{\"a\":1,,}").is_err());
        assert!(parse("[1] x").is_err());
    }

    #[test]
    fn layouts_are_detected_and_written() {
        let v = parse(r#"{"a":{"b":[1,2]},"c":[]}"#).unwrap();
        assert_eq!(Layout::of("{\n  \"a\": 1\n}", false), Layout::Indent("  ".into()));
        assert_eq!(Layout::of("{\n\t\"a\": 1\n}", false), Layout::Indent("\t".into()));
        assert_eq!(Layout::of("{\"a\": 1, \"b\": 2}", false), Layout::Spaced);
        assert_eq!(Layout::of("{\"a\":1}", false), Layout::Compact);
        assert_eq!(dump(&v, &Layout::Spaced), r#"{"a": {"b": [1, 2]}, "c": []}"#);
        assert_eq!(dump(&v, &Layout::Indent("  ".into())), "{\n  \"a\": {\n    \"b\": [\n      1,\n      2\n    ]\n  },\n  \"c\": []\n}");
    }
}
