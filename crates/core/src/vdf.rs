//! Minimal parser for Valve's KeyValues text format (`libraryfolders.vdf`,
//! `appmanifest_*.acf`). Only what Steam writes for these files is supported:
//! quoted keys/values, nested `{}` blocks and `//` comments. [`set_value`]
//! edits one value in place and leaves the rest of the file byte for byte.

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
    tokenize_spans(src).into_iter().map(|(t, _)| t).collect()
}

/// Tokens with the byte range each one covers (quotes included).
fn tokenize_spans(src: &str) -> Vec<(Tok, std::ops::Range<usize>)> {
    let mut out = Vec::new();
    let mut chars = src.char_indices().peekable();
    let end_of = |chars: &mut std::iter::Peekable<std::str::CharIndices>| chars.peek().map(|(i, _)| *i).unwrap_or(src.len());
    while let Some((start, c)) = chars.next() {
        match c {
            '{' => out.push((Tok::Open, start..start + 1)),
            '}' => out.push((Tok::Close, start..start + 1)),
            '/' if chars.peek().map(|(_, c)| *c) == Some('/') => {
                for (_, c) in chars.by_ref() {
                    if c == '\n' {
                        break;
                    }
                }
            }
            '"' => {
                let mut s = String::new();
                let mut chars_iter = std::iter::from_fn(|| chars.next().map(|(_, c)| c));
                while let Some(c) = chars_iter.next() {
                    match c {
                        '\\' => match chars_iter.next() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some(o) => s.push(o),
                            None => break,
                        },
                        '"' => break,
                        o => s.push(o),
                    }
                }
                out.push((Tok::Str(s), start..end_of(&mut chars)));
            }
            c if c.is_whitespace() => {}
            c => {
                // Unquoted token.
                let mut s = String::from(c);
                while let Some(&(_, n)) = chars.peek() {
                    if n.is_whitespace() || n == '{' || n == '}' || n == '"' {
                        break;
                    }
                    s.push(n);
                    chars.next();
                }
                out.push((Tok::Str(s), start..end_of(&mut chars)));
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

/// Quote a string the way Steam writes it.
pub fn quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Set `path`'s last key to `value`, editing the text in place. The last
/// `create` keys may be missing and are added; anything above them must
/// exist, or `None` is returned. Keys match case-insensitively.
pub fn set_value(src: &str, path: &[&str], value: &str, create: usize) -> Option<String> {
    let toks = tokenize_spans(src);
    // Range of tokens inside the current block (exclusive of its braces).
    let (mut lo, mut hi) = (0usize, toks.len());
    let mut insert_at = 0usize; // byte offset to add missing keys at
    let mut depth_indent = String::new();
    for (level, key) in path.iter().enumerate() {
        let last = level + 1 == path.len();
        match find_child(&toks, lo, hi, key) {
            Some(i) => match toks.get(i + 1) {
                Some((Tok::Str(_), r)) if last => {
                    let mut out = String::with_capacity(src.len() + value.len());
                    out.push_str(&src[..r.start]);
                    out.push_str(&quote(value));
                    out.push_str(&src[r.end..]);
                    return Some(out);
                }
                Some((Tok::Open, r)) if !last => {
                    let close = matching_close(&toks, i + 1)?;
                    lo = i + 2;
                    hi = close;
                    insert_at = r.end;
                    depth_indent = line_indent(src, toks[i].1.start) + "\t";
                }
                _ => return None,
            },
            None => {
                if path.len() - level > create || level == 0 {
                    return None;
                }
                // Build the missing keys as nested blocks.
                let mut text = String::new();
                let mut indent = depth_indent.clone();
                let rest = &path[level..];
                for (j, k) in rest.iter().enumerate() {
                    if j + 1 == rest.len() {
                        text.push_str(&format!("\n{indent}{}\t\t{}", quote(k), quote(value)));
                    } else {
                        text.push_str(&format!("\n{indent}{}\n{indent}{{", quote(k)));
                        indent.push('\t');
                    }
                }
                for _ in 1..rest.len() {
                    indent.pop();
                    text.push_str(&format!("\n{indent}}}"));
                }
                let mut out = String::with_capacity(src.len() + text.len());
                out.push_str(&src[..insert_at]);
                out.push_str(&text);
                out.push_str(&src[insert_at..]);
                return Some(out);
            }
        }
    }
    None
}

fn line_indent(src: &str, at: usize) -> String {
    let line_start = src[..at].rfind('\n').map(|i| i + 1).unwrap_or(0);
    src[line_start..at].chars().take_while(|c| c.is_whitespace()).collect()
}

/// Index of the key token named `key` directly inside tokens `lo..hi`.
fn find_child(toks: &[(Tok, std::ops::Range<usize>)], lo: usize, hi: usize, key: &str) -> Option<usize> {
    let mut i = lo;
    while i < hi {
        match &toks[i].0 {
            Tok::Str(k) => {
                if k.eq_ignore_ascii_case(key) {
                    return Some(i);
                }
                match toks.get(i + 1).map(|t| &t.0) {
                    Some(Tok::Open) => i = matching_close(toks, i + 1)? + 1,
                    _ => i += 2,
                }
            }
            _ => i += 1,
        }
    }
    None
}

fn matching_close(toks: &[(Tok, std::ops::Range<usize>)], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (i, (t, _)) in toks.iter().enumerate().skip(open) {
        match t {
            Tok::Open => depth += 1,
            Tok::Close => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
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

    const LOCALCONFIG: &str = "\"UserLocalConfigStore\"\n{\n\t\"Software\"\n\t{\n\t\t\"Valve\"\n\t\t{\n\t\t\t\"Steam\"\n\t\t\t{\n\t\t\t\t\"apps\"\n\t\t\t\t{\n\t\t\t\t\t\"292030\"\n\t\t\t\t\t{\n\t\t\t\t\t\t\"LastPlayed\"\t\t\"1\"\n\t\t\t\t\t}\n\t\t\t\t}\n\t\t\t}\n\t\t}\n\t}\n\t\"friends\" { \"x\" \"y\" }\n}\n";
    const PATH: &[&str] = &["UserLocalConfigStore", "Software", "Valve", "Steam", "apps", "1091500", "LaunchOptions"];

    fn launch(src: &str) -> Option<String> {
        parse(src).get("UserLocalConfigStore")?.get("Software")?.get("Valve")?.get("Steam")?.get("apps")?.get("1091500")?.get("LaunchOptions")?.as_str().map(String::from)
    }

    #[test]
    fn adds_and_replaces_launch_options_in_place() {
        let opts = r#"WINEDLLOVERRIDES="winmm,version=n,b" %command%"#;
        let added = set_value(LOCALCONFIG, PATH, opts, 2).unwrap();
        assert_eq!(launch(&added).as_deref(), Some(opts));
        assert!(added.contains(r#""WINEDLLOVERRIDES=\"winmm,version=n,b\" %command%""#), "{added}");
        assert!(added.contains("\"292030\"") && added.contains("\"friends\""), "rest kept");
        assert_eq!(parse(&added).get("UserLocalConfigStore").unwrap().get("friends").unwrap().get("x").unwrap().as_str(), Some("y"));

        let replaced = set_value(&added, PATH, "-modded", 2).unwrap();
        assert_eq!(launch(&replaced).as_deref(), Some("-modded"));
        assert_eq!(replaced.len(), added.len() - quote(opts).len() + quote("-modded").len(), "only the value changed");

        // Without an apps block Steam hasn't set this account up; don't guess.
        assert!(set_value("\"UserLocalConfigStore\" { }", PATH, "x", 2).is_none());
    }
}
