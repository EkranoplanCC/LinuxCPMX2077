//! Nexus mod descriptions (BBCode mixed with a little HTML) turned into a
//! small tree of known-safe elements, so the app can show them formatted
//! the way the website does without ever handing Nexus' markup to the web
//! view. Anything not on the list below is dropped (its text kept); links
//! are https/http only; images are only shown from Nexus' own image host,
//! everything else becomes a placeholder the user can open in the browser.

use serde::Serialize;

const MAX_INPUT_CHARS: usize = 60_000;
const MAX_NODES: usize = 20_000;
const MAX_DEPTH: usize = 40;
const MAX_TAG_LEN: usize = 600;

/// One piece of a formatted description.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Node {
    Text {
        text: String,
    },
    Br,
    Hr,
    /// `tag` is one of [`TAGS`].
    El {
        tag: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        color: Option<String>,
        /// Nexus' font size, 1 to 7.
        #[serde(skip_serializing_if = "Option::is_none")]
        size: Option<u8>,
        children: Vec<Node>,
    },
    Link {
        href: String,
        children: Vec<Node>,
    },
    /// `inline` images come from Nexus' image host and may be shown; the
    /// rest are only offered as a link.
    Image {
        src: String,
        host: String,
        inline: bool,
    },
    Video {
        url: String,
    },
}

/// Every element tag the tree can contain.
pub const TAGS: &[&str] = &[
    "b", "i", "u", "s", "sup", "sub", "h2", "h3", "h4", "quote", "code", "spoiler", "ul", "ol", "li", "center", "right",
    "p", "div", "span", "color", "size", "table", "tr", "td", "th",
];

/// The link if it is an absolute https/http URL.
pub fn safe_link(u: &str) -> Option<String> {
    let url = url::Url::parse(u.trim()).ok()?;
    (matches!(url.scheme(), "https" | "http") && url.host_str().is_some()).then(|| url.to_string())
}

fn image(u: &str) -> Option<Node> {
    let src = safe_link(u)?;
    let url = url::Url::parse(&src).ok()?;
    let host = url.host_str()?.to_ascii_lowercase();
    let inline = url.scheme() == "https" && host == "staticdelivery.nexusmods.com";
    Some(Node::Image { src, host, inline })
}

/// A YouTube video id or link as a watch link.
fn youtube(s: &str) -> Option<Node> {
    let s = s.trim();
    let id_ok = |id: &str| (6..=20).contains(&id.len()) && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if id_ok(s) {
        return Some(Node::Video { url: format!("https://www.youtube.com/watch?v={s}") });
    }
    let url = safe_link(s)?;
    let host = url::Url::parse(&url).ok()?.host_str()?.to_ascii_lowercase();
    (host.ends_with("youtube.com") || host == "youtu.be").then_some(Node::Video { url })
}

fn color(v: &str) -> Option<String> {
    let v = v.trim().trim_matches(['"', '\'']).to_ascii_lowercase();
    let hex = v.strip_prefix('#').is_some_and(|h| matches!(h.len(), 3 | 6) && h.chars().all(|c| c.is_ascii_hexdigit()));
    let named = (3..=20).contains(&v.len()) && v.chars().all(|c| c.is_ascii_lowercase());
    (hex || named).then_some(v)
}

fn size(v: &str) -> Option<u8> {
    let n: u32 = v.trim().trim_matches(['"', '\'']).trim_end_matches("px").parse().ok()?;
    // Nexus sizes are 1-7; a few authors write pixel sizes instead.
    Some(match n {
        0 => return None,
        1..=7 => n as u8,
        8..=11 => 1,
        12..=13 => 2,
        14..=16 => 3,
        17..=20 => 4,
        21..=26 => 5,
        27..=34 => 6,
        _ => 7,
    })
}

/// What a tag name (BBCode or HTML) becomes.
enum Kind {
    El(&'static str),
    Link,
    Img,
    Video,
    /// `[list]`: `ol` with an argument, `ul` without.
    List,
    Item,
    Br,
    Hr,
    /// Contents are not text (scripts, embeds).
    Drop,
    /// Formatting we don't keep; the text inside stays.
    Transparent,
}

fn kind(name: &str, html: bool) -> Option<Kind> {
    use Kind::*;
    Some(match name {
        "b" | "strong" => El("b"),
        "i" | "em" => El("i"),
        "u" | "ins" => El("u"),
        "s" | "strike" | "del" => El("s"),
        "sup" => El("sup"),
        "sub" => El("sub"),
        "h1" | "h2" | "heading" => El("h2"),
        "h3" => El("h3"),
        "h4" | "h5" | "h6" => El("h4"),
        "quote" | "blockquote" => El("quote"),
        "code" | "pre" => El("code"),
        "spoiler" => El("spoiler"),
        "ul" => El("ul"),
        "ol" => El("ol"),
        "li" if html => Item,
        "*" if !html => Item,
        "list" if !html => List,
        "center" => El("center"),
        "right" => El("right"),
        "left" | "justify" | "font" | "email" | "span" | "indent" => Transparent,
        "p" if html => El("p"),
        "div" if html => El("div"),
        "color" if !html => El("color"),
        "size" if !html => El("size"),
        "table" => El("table"),
        "tr" => El("tr"),
        "td" => El("td"),
        "th" => El("th"),
        "url" if !html => Link,
        "a" if html => Link,
        "img" => Img,
        "youtube" | "video" => Video,
        "br" if html => Br,
        "line" | "hr" => Hr,
        "script" | "style" | "iframe" | "object" | "embed" | "noscript" | "template" | "svg" | "math" if html => Drop,
        _ if html => Transparent,
        _ => return None,
    })
}

/// An HTML attribute's value: `name="v"`, `name='v'` or `name=v`.
fn html_attr(attrs: &str, name: &str) -> Option<String> {
    let lower = attrs.to_ascii_lowercase();
    let mut from = 0;
    while let Some(i) = lower[from..].find(name) {
        let at = from + i;
        from = at + name.len();
        let before_ok = at == 0 || lower.as_bytes()[at - 1].is_ascii_whitespace();
        let rest = attrs[from..].trim_start();
        if !before_ok || !rest.starts_with('=') {
            continue;
        }
        let v = rest[1..].trim_start();
        return Some(match v.chars().next()? {
            q @ ('"' | '\'') => v[1..].split(q).next()?.to_string(),
            _ => v.split(|c: char| c.is_whitespace() || c == '>').next()?.to_string(),
        });
    }
    None
}

struct Open {
    name: String,
    kind: Kind,
    arg: Option<String>,
    color: Option<String>,
    size: Option<u8>,
    children: Vec<Node>,
}

struct Builder {
    stack: Vec<Open>,
    root: Vec<Node>,
    count: usize,
}

impl Builder {
    fn children(&mut self) -> &mut Vec<Node> {
        match self.stack.last_mut() {
            Some(o) => &mut o.children,
            None => &mut self.root,
        }
    }

    fn push(&mut self, n: Node) {
        if self.count >= MAX_NODES {
            return;
        }
        self.count += 1;
        if let Node::Text { text } = &n
            && let Some(Node::Text { text: prev }) = self.children().last_mut()
        {
            prev.push_str(text);
            return;
        }
        self.children().push(n);
    }

    fn open(&mut self, o: Open) {
        if self.stack.len() < MAX_DEPTH {
            self.stack.push(o);
        }
    }

    /// Close the innermost open `name`, and anything opened inside it.
    fn close(&mut self, name: &str) {
        let Some(at) = self.stack.iter().rposition(|o| o.name == name) else { return };
        while self.stack.len() > at {
            self.finish_top();
        }
    }

    fn finish_top(&mut self) {
        let Some(o) = self.stack.pop() else { return };
        let text = || plain(&o.children);
        let node = match o.kind {
            Kind::El(tag) => Some(Node::El { tag, color: o.color, size: o.size, children: o.children }),
            Kind::List => Some(Node::El { tag: if o.arg.is_some() { "ol" } else { "ul" }, color: None, size: None, children: o.children }),
            Kind::Item => Some(Node::El { tag: "li", color: None, size: None, children: o.children }),
            Kind::Link => match o.arg.as_deref().and_then(safe_link).or_else(|| safe_link(&text())) {
                Some(href) => Some(Node::Link { href, children: o.children }),
                None => {
                    for c in o.children {
                        self.push(c);
                    }
                    None
                }
            },
            Kind::Img => o.arg.as_deref().and_then(image).or_else(|| image(&text())),
            Kind::Video => youtube(&text()).or_else(|| o.arg.as_deref().and_then(youtube)),
            Kind::Transparent => {
                for c in o.children {
                    self.push(c);
                }
                None
            }
            Kind::Br | Kind::Hr | Kind::Drop => None,
        };
        if let Some(n) = node {
            self.push(n);
        }
    }
}

/// The text inside `nodes`, for `[url]https://...[/url]` and `[img]`.
fn plain(nodes: &[Node]) -> String {
    let mut s = String::new();
    for n in nodes {
        match n {
            Node::Text { text } => s.push_str(text),
            Node::El { children, .. } | Node::Link { children, .. } => s.push_str(&plain(children)),
            _ => {}
        }
    }
    s.trim().to_string()
}

/// A parsed `[tag=arg]`, `[/tag]`, `<tag attrs>` or `</tag>`.
struct Tag<'a> {
    name: String,
    closing: bool,
    html: bool,
    rest: &'a str,
}

fn parse_tag(inner: &str, html: bool) -> Option<Tag<'_>> {
    let closing = inner.starts_with('/');
    let body = inner.trim_start_matches('/');
    let name: String = body
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '*')
        .collect::<String>()
        .to_ascii_lowercase();
    if name.is_empty() {
        return None;
    }
    let rest = &body[name.len()..];
    // `[b]`, `[url=...]`, `<a href=...>`, `<br/>`; not `[b-side]`.
    let next = rest.chars().next();
    let ok = match next {
        None => true,
        Some('=') => !html,
        Some(c) => c.is_whitespace() || c == '/',
    };
    ok.then_some(Tag { name, closing, html, rest })
}

/// A Nexus description as formatted, safe-to-show nodes.
pub fn to_nodes(src: &str) -> Vec<Node> {
    let src: String = src.chars().take(MAX_INPUT_CHARS).collect();
    let mut b = Builder { stack: vec![], root: vec![], count: 0 };
    let mut rest = src.as_str();
    let mut text = String::new();
    let flush = |b: &mut Builder, text: &mut String| {
        if !text.is_empty() {
            b.push(Node::Text { text: decode(text) });
            text.clear();
        }
    };
    while let Some(c) = rest.chars().next() {
        let close = match c {
            '[' => ']',
            '<' => '>',
            _ => {
                // Line breaks in the source are only layout; Nexus shows <br />.
                text.push(if c == '\n' || c == '\r' { ' ' } else { c });
                rest = &rest[c.len_utf8()..];
                continue;
            }
        };
        let html = c == '<';
        let tag = crate::nexus_browse::head(rest, MAX_TAG_LEN)
            .find(close)
            .and_then(|end| Some((end, parse_tag(&rest[1..end], html)?)))
            .and_then(|(end, t)| Some((end, kind(&t.name, t.html)?, t)));
        let Some((end, k, t)) = tag else {
            text.push(c);
            rest = &rest[1..];
            continue;
        };
        rest = &rest[end + 1..];
        flush(&mut b, &mut text);
        if t.closing {
            let name = if t.name == "*" { "*" } else { t.name.as_str() };
            b.close(name);
            continue;
        }
        let arg = if t.html {
            match k {
                Kind::Link => html_attr(t.rest, "href"),
                Kind::Img => html_attr(t.rest, "src"),
                _ => None,
            }
        } else {
            t.rest.strip_prefix('=').map(|a| decode(a.trim().trim_matches(['"', '\''])))
        };
        match k {
            Kind::Br => b.push(Node::Br),
            Kind::Hr => b.push(Node::Hr),
            Kind::Img if t.html => {
                if let Some(n) = arg.as_deref().and_then(image) {
                    b.push(n);
                }
            }
            Kind::Drop => {
                // Skip to the matching end tag.
                let end_tag = format!("</{}", t.name);
                let lower = rest.to_ascii_lowercase();
                rest = match lower.find(&end_tag) {
                    Some(i) => {
                        let after = &rest[i..];
                        &after[after.find('>').map_or(after.len(), |j| j + 1)..]
                    }
                    None => "",
                };
            }
            Kind::Item => {
                // A new item ends the previous one in the same list.
                if let Some(at) = b.stack.iter().rposition(|o| matches!(o.kind, Kind::List | Kind::Item) || o.name == "ul" || o.name == "ol")
                    && matches!(b.stack[at].kind, Kind::Item)
                {
                    let name = b.stack[at].name.clone();
                    b.close(&name);
                }
                b.open(Open { name: t.name, kind: k, arg: None, color: None, size: None, children: vec![] });
            }
            k => {
                let (color, size) = match t.name.as_str() {
                    "color" => (arg.as_deref().and_then(color), None),
                    "size" => (None, arg.as_deref().and_then(size)),
                    _ => (None, None),
                };
                // [color] or [size] with a value we won't use is just text styling we drop.
                let k = match (t.name.as_str(), &color, size) {
                    ("color", None, _) | ("size", _, None) => Kind::Transparent,
                    _ => k,
                };
                b.open(Open { name: t.name, kind: k, arg, color, size, children: vec![] });
            }
        }
    }
    flush(&mut b, &mut text);
    while !b.stack.is_empty() {
        b.finish_top();
    }
    b.root
}

fn decode(s: &str) -> String {
    crate::nexus_browse::decode_entities(s).chars().filter(|c| !c.is_control() || *c == '\n').collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn tree(src: &str) -> Value {
        serde_json::to_value(to_nodes(src)).unwrap()
    }

    #[test]
    fn keeps_formatting_and_structure() {
        let v = tree("[b][color=#f3e500][size=5]ARCHIVE-XL[/size][/color][/b]\n<br />Text &amp; more");
        assert_eq!(
            v,
            json!([
                {"t":"el","tag":"b","children":[{"t":"el","tag":"color","color":"#f3e500","children":[
                    {"t":"el","tag":"size","size":5,"children":[{"t":"text","text":"ARCHIVE-XL"}]}]}]},
                {"t":"text","text":" "},
                {"t":"br"},
                {"t":"text","text":"Text & more"}
            ])
        );
    }

    #[test]
    fn lists_items_and_links() {
        let v = tree("[list=1][*]one [*][url=https://www.nexusmods.com/cyberpunk2077/mods/2380]RED4ext[/url][/list][url]https://x.example/a[/url]");
        assert_eq!(v[0]["tag"], "ol");
        let items = v[0]["children"].as_array().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["children"][0]["text"], "one ");
        assert_eq!(items[1]["children"][0]["t"], "link");
        assert_eq!(items[1]["children"][0]["href"], "https://www.nexusmods.com/cyberpunk2077/mods/2380");
        assert_eq!(v[1]["href"], "https://x.example/a");
        assert_eq!(tree("[list][*]a[/list]")[0]["tag"], "ul");
    }

    #[test]
    fn unsafe_things_never_get_through() {
        let v = tree(
            "<script>alert(1)</script>[url=javascript:alert(1)]x[/url]<a href=\"data:text/html,hi\">y</a>\
             <img src=x onerror=alert(1)>[color=red;background:url(x)]z[/color]<iframe src=https://e.example></iframe>\
             <div onclick=\"evil()\">w</div>[size=999px]big[/size]",
        );
        let s = v.to_string();
        for bad in ["alert", "javascript", "data:", "onerror", "background", "iframe", "onclick", "evil"] {
            assert!(!s.contains(bad), "{bad} in {s}");
        }
        // The visible text survives.
        assert!(s.contains("\"xyz\"") && s.contains("\"w\""), "{s}");
        assert!(s.contains("\"size\":7"), "{s}");
        // Every element tag is on the list.
        fn tags(v: &Value, out: &mut Vec<String>) {
            if let Some(t) = v.get("tag").and_then(Value::as_str) {
                out.push(t.into());
            }
            for c in v.get("children").and_then(Value::as_array).into_iter().flatten().chain(v.as_array().into_iter().flatten()) {
                tags(c, out);
            }
        }
        let mut seen = vec![];
        tags(&v, &mut seen);
        assert!(seen.iter().all(|t| TAGS.contains(&t.as_str())), "{seen:?}");
    }

    #[test]
    fn images_are_only_inline_from_nexus() {
        let v = tree("[img]https://staticdelivery.nexusmods.com/mods/3333/images/1.png[/img][img]https://i.imgur.com/a.png[/img][img]ftp://x/y[/img]");
        assert_eq!(v[0], json!({"t":"image","src":"https://staticdelivery.nexusmods.com/mods/3333/images/1.png","host":"staticdelivery.nexusmods.com","inline":true}));
        assert_eq!(v[1]["inline"], false);
        assert_eq!(v[1]["host"], "i.imgur.com");
        assert_eq!(v.as_array().unwrap().len(), 2, "non-web image dropped");
        assert_eq!(tree("[youtube]dQw4w9WgXcQ[/youtube]")[0]["url"], "https://www.youtube.com/watch?v=dQw4w9WgXcQ");
    }

    #[test]
    fn unknown_brackets_stay_text_and_unclosed_tags_close() {
        assert_eq!(tree("a [notatag] b [")[0]["text"], "a [notatag] b [");
        let v = tree("[b]bold [i]both");
        assert_eq!(v[0]["tag"], "b");
        assert_eq!(v[0]["children"][1]["tag"], "i");
        // A stray closing tag is ignored.
        assert_eq!(tree("x[/b]y")[0]["text"], "xy");
        // Deep nesting is capped, not a stack overflow.
        let deep = "[b]".repeat(10_000) + "x";
        assert!(tree(&deep).to_string().contains("\"x\""));
        // Multi-byte characters next to the tag length limit.
        let wide = format!("[{}— x", "a".repeat(MAX_TAG_LEN - 2));
        assert_eq!(tree(&wide)[0]["text"].as_str().unwrap().chars().count(), wide.chars().count());
    }
}
