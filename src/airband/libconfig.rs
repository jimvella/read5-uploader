//! A reader for libconfig files, the format of `rtl_airband.conf`.
//!
//! Covers the grammar libconfig 1.7 accepts: settings `name = value` or
//! `name : value` ended by `;`, `,` or nothing; groups `{}`; lists `()`;
//! arrays `[]`; strings with escapes, adjacent strings joined; integers
//! (decimal, hex, binary, octal, optional `L`), floats, booleans; `#`, `//`
//! and `/* */` comments; `@include "file"`. Values are kept with their line
//! number so callers can name the line a warning is about.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Group(Vec<Setting>),
    List(Vec<Value>),
    Array(Vec<Value>),
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Value {
    pub kind: Kind,
    pub line: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    pub name: String,
    pub value: Value,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{file}:{line}: {msg}")]
    Syntax { file: String, line: u32, msg: String },
    #[error("cannot read {path}: {source}")]
    Io { path: String, source: std::io::Error },
}

impl Value {
    /// Look a setting up by name, when this is a group.
    pub fn get(&self, name: &str) -> Option<&Value> {
        match &self.kind {
            Kind::Group(s) => s.iter().find(|s| s.name == name).map(|s| &s.value),
            _ => None,
        }
    }

    /// Elements of a list or array.
    pub fn items(&self) -> &[Value] {
        match &self.kind {
            Kind::List(v) | Kind::Array(v) => v,
            _ => &[],
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match &self.kind {
            Kind::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self.kind {
            Kind::Bool(b) => Some(b),
            _ => None,
        }
    }

    #[cfg(test)]
    pub fn as_int(&self) -> Option<i64> {
        match self.kind {
            Kind::Int(i) => Some(i),
            _ => None,
        }
    }
}

/// Parse a file, following `@include` relative to the including file's
/// directory (then the working directory).
pub fn parse_file(path: &Path) -> Result<Value, Error> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    let src = expand_includes(&text, path, 0)?;
    parse_named(&src, &path.display().to_string())
}

/// Parse text with no includes (an `@include` line is an error).
#[cfg(test)]
pub fn parse(text: &str) -> Result<Value, Error> {
    parse_named(text, "<config>")
}

/// `@include` is textual in libconfig, so expand it before tokenising. Line
/// numbers after an include count the included lines too.
fn expand_includes(text: &str, path: &Path, depth: u32) -> Result<String, Error> {
    if depth > 10 {
        return Err(Error::Syntax {
            file: path.display().to_string(),
            line: 0,
            msg: "@include nested more than 10 deep".into(),
        });
    }
    let mut out = String::with_capacity(text.len());
    for (i, line) in text.lines().enumerate() {
        let t = line.trim_start();
        if let Some(rest) = t.strip_prefix("@include") {
            let name = rest.trim().trim_matches('"');
            let base = path.parent().unwrap_or(Path::new("."));
            let mut inc = base.join(name);
            if !inc.exists() {
                inc = PathBuf::from(name);
            }
            let body = std::fs::read_to_string(&inc).map_err(|e| Error::Io {
                path: format!("{} (included at {}:{})", inc.display(), path.display(), i + 1),
                source: e,
            })?;
            out.push_str(&expand_includes(&body, &inc, depth + 1)?);
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    Ok(out)
}

fn parse_named(text: &str, file: &str) -> Result<Value, Error> {
    let mut p = Parser { s: text.as_bytes(), i: 0, line: 1, file };
    let settings = p.settings(None)?;
    p.ws()?;
    if p.i < p.s.len() {
        return Err(p.err("unexpected text"));
    }
    Ok(Value { kind: Kind::Group(settings), line: 1 })
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
    line: u32,
    file: &'a str,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: &str) -> Error {
        Error::Syntax { file: self.file.to_string(), line: self.line, msg: msg.to_string() }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let c = self.peek()?;
        self.i += 1;
        if c == b'\n' {
            self.line += 1;
        }
        Some(c)
    }

    /// Skip whitespace and comments.
    fn ws(&mut self) -> Result<(), Error> {
        loop {
            match self.peek() {
                Some(c) if c.is_ascii_whitespace() => {
                    self.bump();
                }
                Some(b'#') => self.skip_line(),
                Some(b'/') if self.s.get(self.i + 1) == Some(&b'/') => self.skip_line(),
                Some(b'/') if self.s.get(self.i + 1) == Some(&b'*') => {
                    self.i += 2;
                    loop {
                        match self.bump() {
                            None => return Err(self.err("unterminated /* comment")),
                            Some(b'*') if self.peek() == Some(b'/') => {
                                self.i += 1;
                                break;
                            }
                            _ => {}
                        }
                    }
                }
                _ => return Ok(()),
            }
        }
    }

    fn skip_line(&mut self) {
        while let Some(c) = self.peek() {
            if c == b'\n' {
                break;
            }
            self.i += 1;
        }
    }

    /// Settings until `close` (or end of input when `None`).
    fn settings(&mut self, close: Option<u8>) -> Result<Vec<Setting>, Error> {
        let mut out = Vec::new();
        loop {
            self.ws()?;
            match (self.peek(), close) {
                (None, None) => return Ok(out),
                (None, Some(_)) => return Err(self.err("unexpected end of file inside a group")),
                (Some(c), Some(cl)) if c == cl => {
                    self.bump();
                    return Ok(out);
                }
                _ => {}
            }
            let name = self.name()?;
            self.ws()?;
            match self.bump() {
                Some(b'=') | Some(b':') => {}
                _ => return Err(self.err(&format!("expected '=' or ':' after {name}"))),
            }
            self.ws()?;
            let value = self.value()?;
            self.ws()?;
            if matches!(self.peek(), Some(b';') | Some(b',')) {
                self.bump();
            }
            out.push(Setting { name, value });
        }
    }

    fn name(&mut self) -> Result<String, Error> {
        let start = self.i;
        match self.peek() {
            Some(c) if c.is_ascii_alphabetic() || c == b'*' => {}
            _ => return Err(self.err("expected a setting name")),
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == b'_' || c == b'-' || c == b'*' {
                self.i += 1;
            } else {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&self.s[start..self.i]).into_owned())
    }

    fn value(&mut self) -> Result<Value, Error> {
        let line = self.line;
        let kind = match self.peek() {
            Some(b'{') => {
                self.bump();
                Kind::Group(self.settings(Some(b'}'))?)
            }
            Some(b'(') => {
                self.bump();
                Kind::List(self.elements(b')')?)
            }
            Some(b'[') => {
                self.bump();
                Kind::Array(self.elements(b']')?)
            }
            Some(b'"') => {
                let mut s = String::new();
                loop {
                    s.push_str(&self.string()?);
                    self.ws()?;
                    if self.peek() != Some(b'"') {
                        break;
                    }
                }
                Kind::Str(s)
            }
            Some(_) => self.scalar()?,
            None => return Err(self.err("expected a value")),
        };
        Ok(Value { kind, line })
    }

    fn elements(&mut self, close: u8) -> Result<Vec<Value>, Error> {
        let mut out = Vec::new();
        loop {
            self.ws()?;
            if self.peek() == Some(close) {
                self.bump();
                return Ok(out);
            }
            out.push(self.value()?);
            self.ws()?;
            match self.peek() {
                Some(b',') => {
                    self.bump();
                }
                Some(c) if c == close => {}
                None => return Err(self.err("unexpected end of file inside a list")),
                _ => return Err(self.err("expected ',' between list elements")),
            }
        }
    }

    fn string(&mut self) -> Result<String, Error> {
        self.bump(); // opening quote
        let mut out: Vec<u8> = Vec::new();
        loop {
            match self.bump() {
                None => return Err(self.err("unterminated string")),
                Some(b'"') => break,
                Some(b'\\') => match self.bump() {
                    Some(b'n') => out.push(b'\n'),
                    Some(b'r') => out.push(b'\r'),
                    Some(b't') => out.push(b'\t'),
                    Some(b'f') => out.push(0x0c),
                    Some(b'\\') => out.push(b'\\'),
                    Some(b'"') => out.push(b'"'),
                    Some(b'x') => {
                        let h = self.s.get(self.i..self.i + 2).ok_or_else(|| self.err("bad \\x escape"))?;
                        let v = u8::from_str_radix(std::str::from_utf8(h).unwrap_or(""), 16)
                            .map_err(|_| self.err("bad \\x escape"))?;
                        self.i += 2;
                        out.push(v);
                    }
                    _ => return Err(self.err("unknown escape in string")),
                },
                Some(c) => out.push(c),
            }
        }
        Ok(String::from_utf8_lossy(&out).into_owned())
    }

    fn scalar(&mut self) -> Result<Kind, Error> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || matches!(c, b'.' | b'+' | b'-' | b'_') {
                self.i += 1;
            } else {
                break;
            }
        }
        let tok = std::str::from_utf8(&self.s[start..self.i]).unwrap_or("");
        if tok.is_empty() {
            return Err(self.err("expected a value"));
        }
        if tok.eq_ignore_ascii_case("true") {
            return Ok(Kind::Bool(true));
        }
        if tok.eq_ignore_ascii_case("false") {
            return Ok(Kind::Bool(false));
        }
        parse_number(tok).ok_or_else(|| self.err(&format!("bad value {tok:?}")))
    }
}

fn parse_number(tok: &str) -> Option<Kind> {
    let t = tok.trim_end_matches(['L', 'l']);
    let t = t.strip_suffix("LL").unwrap_or(t);
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let radix = |p: &str, r: u32| i64::from_str_radix(&body[p.len()..], r).ok();
    let lower = body.to_ascii_lowercase();
    let int = if lower.starts_with("0x") {
        radix("0x", 16)
    } else if lower.starts_with("0b") {
        radix("0b", 2)
    } else if lower.starts_with("0q") {
        radix("0q", 8)
    } else if body.bytes().all(|b| b.is_ascii_digit()) && !body.is_empty() {
        body.parse::<i64>().ok()
    } else {
        None
    };
    if let Some(v) = int {
        return Some(Kind::Int(if neg { -v } else { v }));
    }
    tok.parse::<f64>().ok().filter(|f| f.is_finite()).map(Kind::Float)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_groups_lists_arrays() {
        let v = parse(
            r#"
            # comment
            a = 1; b : 0x10, c = -2.5e1
            d = "x" "y";  // joined
            /* block
               comment */
            e = ( { f = true; }, "s" );
            g = [ 1, 2, 3 ];
            h = 25L;
            i = 118.15
            "#,
        )
        .unwrap();
        assert_eq!(v.get("a").unwrap().as_int(), Some(1));
        assert_eq!(v.get("b").unwrap().as_int(), Some(16));
        assert_eq!(v.get("c").unwrap().kind, Kind::Float(-25.0));
        assert_eq!(v.get("d").unwrap().as_str(), Some("xy"));
        let e = v.get("e").unwrap().items();
        assert_eq!(e[0].get("f").unwrap().as_bool(), Some(true));
        assert_eq!(e[1].as_str(), Some("s"));
        assert_eq!(v.get("g").unwrap().items().len(), 3);
        assert_eq!(v.get("h").unwrap().as_int(), Some(25));
        assert_eq!(v.get("i").unwrap().kind, Kind::Float(118.15));
        assert_eq!(v.get("e").unwrap().line, 7);
    }

    #[test]
    fn errors_name_the_line() {
        let e = parse("a = 1;\nb = ;\n").unwrap_err().to_string();
        assert!(e.contains(":2:"), "{e}");
        assert!(parse("a = { b = 1;").is_err());
        assert!(parse("a = \"open").is_err());
    }

    #[test]
    fn includes_are_expanded() {
        let dir = std::env::temp_dir().join(format!("read5-libconfig-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("inner.conf"), "b = 2;\n").unwrap();
        std::fs::write(dir.join("outer.conf"), "a = 1;\n@include \"inner.conf\"\nc = 3;\n").unwrap();
        let v = parse_file(&dir.join("outer.conf")).unwrap();
        assert_eq!(v.get("b").unwrap().as_int(), Some(2));
        assert_eq!(v.get("c").unwrap().as_int(), Some(3));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn every_upstream_example_parses() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/rtl_airband");
        let mut n = 0;
        for e in std::fs::read_dir(&dir).unwrap() {
            let p = e.unwrap().path();
            if p.extension().map(|x| x == "conf").unwrap_or(false) {
                let v = parse_file(&p).unwrap_or_else(|e| panic!("{e}"));
                assert!(v.get("devices").is_some() || v.get("mixers").is_some(), "{}", p.display());
                n += 1;
            }
        }
        assert!(n >= 8, "found {n} example configs");
    }
}
