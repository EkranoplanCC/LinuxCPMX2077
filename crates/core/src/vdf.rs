//! Minimal parser for Valve's KeyValues text format (`libraryfolders.vdf`,
//! `appmanifest_*.acf`). Only what Steam writes for these files is supported:
//! quoted keys/values, nested `{}` blocks and `//` comments.

use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Obj(BTreeMap<String, Value>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            // Steam keys are case-insensitive in practice.
            Value::Obj(m) => m
                .get(key)
                .or_else(|| m.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v)),
            Value::Str(_) => None,
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            Value::Obj(_) => None,
        }
    }
    pub fn as_obj(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Obj(m) => Some(m),
            Value::Str(_) => None,
        }
    }
}

#[derive(Debug, PartialEq)]
enum Tok {
    Str(String),
    Open,
    Close,
}

fn tokenize(src: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut chars = src.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '{' => out.push(Tok::Open),
            '}' => out.push(Tok::Close),
            '/' if chars.peek() == Some(&'/') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' => {
                let mut s = String::new();
                while let Some(c) = chars.next() {
                    match c {
                        '\\' => match chars.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(o) => s.push(o),
                            None => break,
                        },
                        '"' => break,
                        o => s.push(o),
                    }
                }
                out.push(Tok::Str(s));
            }
            c if c.is_whitespace() => {}
            c => {
                // Unquoted token.
                let mut s = String::from(c);
                while let Some(&n) = chars.peek() {
                    if n.is_whitespace() || n == '{' || n == '}' || n == '"' {
                        break;
                    }
                    s.push(n);
                    chars.next();
                }
                out.push(Tok::Str(s));
            }
        }
    }
    out
}

fn parse_obj(toks: &[Tok], pos: &mut usize) -> BTreeMap<String, Value> {
    let mut map = BTreeMap::new();
    while *pos < toks.len() {
        match &toks[*pos] {
            Tok::Close => {
                *pos += 1;
                return map;
            }
            Tok::Open => {
                *pos += 1; // stray brace, skip
            }
            Tok::Str(k) => {
                let key = k.clone();
                *pos += 1;
                match toks.get(*pos) {
                    Some(Tok::Str(v)) => {
                        map.insert(key, Value::Str(v.clone()));
                        *pos += 1;
                    }
                    Some(Tok::Open) => {
                        *pos += 1;
                        map.insert(key, Value::Obj(parse_obj(toks, pos)));
                    }
                    _ => {}
                }
            }
        }
    }
    map
}

/// Parse a whole file; the result is an object holding the root key(s).
pub fn parse(src: &str) -> Value {
    let toks = tokenize(src);
    let mut pos = 0;
    Value::Obj(parse_obj(&toks, &mut pos))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_libraryfolders() {
        let src = r#"
"libraryfolders"
{
	"0"
	{
		"path"		"/home/v/.local/share/Steam"
		"apps"
		{
			"228980"		"1234"
		}
	}
	"1"
	{
		"path"		"/mnt/games/SteamLibrary"
		"apps" { "1091500" "70000000000" }
	}
}"#;
        let v = parse(src);
        let lf = v.get("libraryfolders").unwrap();
        assert_eq!(lf.get("1").unwrap().get("path").unwrap().as_str(), Some("/mnt/games/SteamLibrary"));
        assert!(lf.get("1").unwrap().get("apps").unwrap().get("1091500").is_some());
    }
}
