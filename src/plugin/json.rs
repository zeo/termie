//! a tiny dependency-free json value, parser, and serializer scoped to the
//! plugin wire protocol. termie stays lean (no serde); the wire is still real
//! json so plugin authors in any language can emit it with their stdlib

use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Num(n) => Some(*n),
            _ => None,
        }
    }

    /// part of the Json accessor surface; not used by the current call sites but
    /// kept for completeness alongside as_str/as_f64/as_array
    #[allow(dead_code)]
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    /// look up a key on an object value
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(m) => m.get(key),
            _ => None,
        }
    }

    /// convenience: a string field on an object
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.get(key).and_then(Json::as_str)
    }

    pub fn obj(pairs: impl IntoIterator<Item = (&'static str, Json)>) -> Json {
        Json::Obj(pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
    }

    pub fn parse(input: &str) -> Option<Json> {
        let bytes = input.as_bytes();
        let mut p = Parser { b: bytes, i: 0 };
        p.skip_ws();
        let v = p.value()?;
        p.skip_ws();
        // trailing garbage is a parse failure
        if p.i != bytes.len() {
            return None;
        }
        Some(v)
    }

    fn write(&self, out: &mut String) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Num(n) => {
                // finite numbers only; non-finite would be invalid json
                if n.is_finite() {
                    let _ = write!(out, "{n}");
                } else {
                    out.push('0');
                }
            }
            Json::Str(s) => write_escaped(s, out),
            Json::Arr(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
            Json::Obj(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_escaped(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }
}

impl std::fmt::Display for Json {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = String::new();
        self.write(&mut s);
        f.write_str(&s)
    }
}

fn write_escaped(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

struct Parser<'a> {
    b: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn skip_ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn value(&mut self) -> Option<Json> {
        self.skip_ws();
        match self.b.get(self.i)? {
            b'{' => self.object(),
            b'[' => self.array(),
            b'"' => self.string().map(Json::Str),
            b't' | b'f' => self.boolean(),
            b'n' => self.null(),
            _ => self.number(),
        }
    }

    fn object(&mut self) -> Option<Json> {
        self.i += 1; // {
        let mut m = BTreeMap::new();
        self.skip_ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Some(Json::Obj(m));
        }
        loop {
            self.skip_ws();
            if self.b.get(self.i)? != &b'"' {
                return None;
            }
            let key = self.string()?;
            self.skip_ws();
            if self.b.get(self.i)? != &b':' {
                return None;
            }
            self.i += 1;
            let val = self.value()?;
            m.insert(key, val);
            self.skip_ws();
            match self.b.get(self.i)? {
                b',' => {
                    self.i += 1;
                }
                b'}' => {
                    self.i += 1;
                    return Some(Json::Obj(m));
                }
                _ => return None,
            }
        }
    }

    fn array(&mut self) -> Option<Json> {
        self.i += 1; // [
        let mut a = Vec::new();
        self.skip_ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Some(Json::Arr(a));
        }
        loop {
            let val = self.value()?;
            a.push(val);
            self.skip_ws();
            match self.b.get(self.i)? {
                b',' => {
                    self.i += 1;
                }
                b']' => {
                    self.i += 1;
                    return Some(Json::Arr(a));
                }
                _ => return None,
            }
        }
    }

    fn string(&mut self) -> Option<String> {
        self.i += 1; // opening quote
        let mut s = String::new();
        loop {
            let c = *self.b.get(self.i)?;
            self.i += 1;
            match c {
                b'"' => return Some(s),
                b'\\' => {
                    let e = *self.b.get(self.i)?;
                    self.i += 1;
                    match e {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'n' => s.push('\n'),
                        b'r' => s.push('\r'),
                        b't' => s.push('\t'),
                        b'b' => s.push('\u{08}'),
                        b'f' => s.push('\u{0c}'),
                        b'u' => {
                            let cp = self.hex4()?;
                            // handle a utf-16 surrogate pair
                            if (0xD800..=0xDBFF).contains(&cp) {
                                if self.b.get(self.i) == Some(&b'\\')
                                    && self.b.get(self.i + 1) == Some(&b'u')
                                {
                                    self.i += 2;
                                    let lo = self.hex4()?;
                                    let c = 0x10000
                                        + ((cp - 0xD800) << 10)
                                        + (lo.checked_sub(0xDC00)?);
                                    s.push(char::from_u32(c)?);
                                } else {
                                    return None;
                                }
                            } else {
                                s.push(char::from_u32(cp)?);
                            }
                        }
                        _ => return None,
                    }
                }
                // raw control chars are invalid inside a json string
                c if c < 0x20 => return None,
                // utf-8 continuation: collect the whole codepoint's bytes
                _ => {
                    let start = self.i - 1;
                    let len = utf8_len(c);
                    let end = start + len;
                    if end > self.b.len() {
                        return None;
                    }
                    self.i = end;
                    s.push_str(std::str::from_utf8(&self.b[start..end]).ok()?);
                }
            }
        }
    }

    fn hex4(&mut self) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..4 {
            let c = *self.b.get(self.i)?;
            self.i += 1;
            let d = match c {
                b'0'..=b'9' => (c - b'0') as u32,
                b'a'..=b'f' => (c - b'a' + 10) as u32,
                b'A'..=b'F' => (c - b'A' + 10) as u32,
                _ => return None,
            };
            v = v * 16 + d;
        }
        Some(v)
    }

    fn boolean(&mut self) -> Option<Json> {
        if self.b[self.i..].starts_with(b"true") {
            self.i += 4;
            Some(Json::Bool(true))
        } else if self.b[self.i..].starts_with(b"false") {
            self.i += 5;
            Some(Json::Bool(false))
        } else {
            None
        }
    }

    fn null(&mut self) -> Option<Json> {
        if self.b[self.i..].starts_with(b"null") {
            self.i += 4;
            Some(Json::Null)
        } else {
            None
        }
    }

    fn number(&mut self) -> Option<Json> {
        let start = self.i;
        while self.i < self.b.len()
            && matches!(self.b[self.i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
        {
            self.i += 1;
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        s.parse::<f64>().ok().map(Json::Num)
    }
}

fn utf8_len(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_primitives() {
        for src in ["null", "true", "false", "0", "-12", "3.5", "\"hi\""] {
            let v = Json::parse(src).expect(src);
            assert_eq!(Json::parse(&v.to_string()).unwrap(), v);
        }
    }

    #[test]
    fn parses_nested_object() {
        let v = Json::parse(r#"{"a":1,"b":[true,null,"x"],"c":{"d":2}}"#).unwrap();
        assert_eq!(v.get("a").and_then(Json::as_f64), Some(1.0));
        assert_eq!(v.get("b").and_then(Json::as_array).map(|a| a.len()), Some(3));
        assert_eq!(
            v.get("c").and_then(|c| c.get("d")).and_then(Json::as_f64),
            Some(2.0)
        );
    }

    #[test]
    fn string_escapes_roundtrip() {
        let s = Json::Str("tab\there\nnnew \"quote\" \\slash\\ \u{1f600}".to_string());
        let out = s.to_string();
        assert_eq!(Json::parse(&out).unwrap(), s);
    }

    #[test]
    fn parses_unicode_escape_and_surrogate_pair() {
        assert_eq!(Json::parse(r#""A""#).unwrap().as_str(), Some("A"));
        // U+1F600 as a surrogate pair
        assert_eq!(
            Json::parse(r#""😀""#).unwrap().as_str(),
            Some("\u{1f600}")
        );
    }

    #[test]
    fn rejects_trailing_garbage_and_unterminated() {
        assert!(Json::parse("{} x").is_none());
        assert!(Json::parse("\"unterminated").is_none());
        assert!(Json::parse("[1,2,").is_none());
        assert!(Json::parse("").is_none());
    }

    #[test]
    fn empty_containers() {
        assert_eq!(Json::parse("{}").unwrap(), Json::Obj(BTreeMap::new()));
        assert_eq!(Json::parse("[]").unwrap(), Json::Arr(vec![]));
        assert_eq!(Json::parse("  [ ]  ").unwrap(), Json::Arr(vec![]));
    }
}
