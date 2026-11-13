//! Rich text on phones (2026-11-12): text messages keep their HTML or RTF
//! next to the plain text. Hosts render the cleaned markup (`display_html`)
//! and write rich text plus plain text to the phone clipboard
//! (`clipboard`). The cleaning follows the transfer page spec
//! (native-rewrite.md「富文本气泡」, 「保留什么、退回什么」):
//!
//! - kept, normalized to one tag each: bold (`b strong`, CSS
//!   `font-weight` ≥ 600 / bold), italic (`i em cite var dfn`, CSS
//!   `font-style: italic`), underline (`u ins`, CSS underline), strike
//!   (`s strike del`, CSS line-through), inline code (`code kbd samp tt`),
//!   `pre`, links (`a href` with http, https, mailto or tel), `h1`–`h6`,
//!   `ul ol li` (`ol start`), `blockquote`, `hr`, `table tr th td` (a table
//!   inside a table keeps only its text), `img` as its `alt` only;
//! - plain blocks: `p`, and `div` for every layout container (`section`,
//!   `article`, `header`, `footer`, `figure`, ...);
//! - tag removed, text kept: every other element (`span font mark small sup
//!   sub` ...), and links with any other scheme;
//! - dropped with their content: scripts, styles, frames, objects, media,
//!   forms and their controls, `head`.
//!
//! No attribute survives but `a href`, `ol start` and `img alt`; nothing is
//! executed and nothing is ever fetched (no image source survives). Content
//! whose cleaning keeps no formatting at all (only colours, fonts, sizes,
//! paragraphs) is plain text: no rich text is handed to hosts for it.
//!
//! RTF is converted to the same markup (Android's clipboard has no RTF type,
//! and hosts render one markup): character formatting, paragraphs, line
//! breaks, tabs, Unicode (`\u`) and single-byte code page escapes (Windows
//! 1252 / Latin-1), and HYPERLINK fields. Double-byte code page escapes
//! (`\'hh` pairs under a CJK `\ansicpg` without `\u`) are not decoded and
//! show as U+FFFD; Cocoa, Word and WordPad write `\u` for such text.
use crate::RichTextFormat;
use crate::store::RichText;

/// Rich text for the phone clipboard. `html` is always present: the source
/// HTML, or HTML converted from RTF. `rtf` is the source RTF when that is
/// what was kept (iOS writes it as `public.rtf` too).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClipboardRich {
    pub html: String,
    pub rtf: Option<String>,
}

impl ClipboardRich {
    /// None when the content keeps no formatting once cleaned: it is plain
    /// text, and only the plain text is written.
    pub fn from_source(format: RichTextFormat, source: String) -> Option<Self> {
        let rich = match format {
            RichTextFormat::Html => Self {
                html: source,
                rtf: None,
            },
            RichTextFormat::Rtf => Self {
                html: rtf_to_html(&source),
                rtf: Some(source),
            },
        };
        sanitize_html(&rich.html).formatted.then_some(rich)
    }
}

/// The kept rich text of a message, read for the clipboard. None when the
/// file is gone or is not UTF-8 (RTF is 7-bit; HTML arrives as UTF-8), and
/// when it keeps no formatting.
pub fn clipboard(rich: &RichText) -> Option<ClipboardRich> {
    let source = std::fs::read_to_string(&rich.path).ok()?;
    ClipboardRich::from_source(rich.format, source)
}

/// The kept rich text of a message as cleaned markup for rendering; None
/// when it keeps no formatting (the message is shown as plain text).
pub fn display_html(rich: &RichText) -> Option<String> {
    let source = std::fs::read_to_string(&rich.path).ok()?;
    let clean = match rich.format {
        RichTextFormat::Html => sanitize_html(&source),
        RichTextFormat::Rtf => sanitize_html(&rtf_to_html(&source)),
    };
    clean.formatted.then_some(clean.html)
}

/// Cleaned markup and whether it keeps any formatting (anything but
/// paragraphs, blocks and line breaks).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sanitized {
    pub html: String,
    pub formatted: bool,
}

/// Elements dropped with everything inside them.
/// (Only elements that have an end tag: the content runs to it.)
const DROPPED: &[&str] = &[
    "script", "style", "head", "title", "template", "iframe", "frameset", "object", "applet", "noscript", "noembed",
    "noframes", "svg", "math", "textarea", "select", "button", "audio", "video", "canvas", "map", "xml", "form",
    "datalist",
];
/// Elements without content or end tag.
const VOID: &[&str] = &[
    "br", "hr", "img", "input", "meta", "link", "wbr", "area", "base", "col", "param", "source", "track", "keygen",
    "embed", "frame",
];
/// Layout containers kept as plain blocks.
const BLOCKS: &[&str] = &[
    "div",
    "section",
    "article",
    "header",
    "footer",
    "figure",
    "figcaption",
    "main",
    "aside",
    "nav",
    "address",
    "center",
    "dl",
    "dt",
    "dd",
    "details",
    "summary",
    "hgroup",
];

/// What a start tag becomes.
enum Start {
    /// Skipped with its content.
    Drop,
    /// A void element's markup (possibly none).
    Void(String),
    /// An element with content: the output tags it opens (in order) and the
    /// first one's attributes. No tag: only its text stays.
    Open(Vec<&'static str>, String),
}

fn start(name: &str, attributes: &str, tables: usize) -> Start {
    let one = |tag: &'static str| Start::Open(vec![tag], String::new());
    match name {
        _ if DROPPED.contains(&name) => Start::Drop,
        "br" => Start::Void("<br>".into()),
        "hr" => Start::Void("<hr>".into()),
        "img" => Start::Void(
            attribute(attributes, "alt")
                .filter(|alt| !alt.trim().is_empty())
                .map_or_else(String::new, |alt| {
                    let mut markup = String::from("<img alt=\"");
                    push_escaped(&mut markup, alt.trim(), true);
                    markup.push_str("\">");
                    markup
                }),
        ),
        _ if VOID.contains(&name) => Start::Void(String::new()),
        "b" | "strong" => one("b"),
        "i" | "em" | "cite" | "var" | "dfn" => one("i"),
        "u" | "ins" => one("u"),
        "s" | "strike" | "del" => one("s"),
        "code" | "kbd" | "samp" | "tt" => one("code"),
        "pre" | "p" | "blockquote" | "ul" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => Start::Open(
            vec![match name {
                "pre" => "pre",
                "p" => "p",
                "blockquote" => "blockquote",
                "ul" => "ul",
                "li" => "li",
                "h1" => "h1",
                "h2" => "h2",
                "h3" => "h3",
                "h4" => "h4",
                "h5" => "h5",
                _ => "h6",
            }],
            String::new(),
        ),
        "ol" => Start::Open(
            vec!["ol"],
            attribute(attributes, "start")
                .filter(|start| !start.is_empty() && start.len() <= 9 && start.bytes().all(|b| b.is_ascii_digit()))
                .map_or_else(String::new, |start| format!(" start=\"{start}\"")),
        ),
        // A link with another scheme is only its text.
        "a" => attribute(attributes, "href")
            .map(|href| href.trim().to_owned())
            .filter(|href| safe_scheme(href))
            .map_or(Start::Open(Vec::new(), String::new()), |href| {
                let mut markup = String::from(" href=\"");
                push_escaped(&mut markup, &href, true);
                markup.push('"');
                Start::Open(vec!["a"], markup)
            }),
        // A table inside a table keeps only its text.
        "table" | "tr" | "th" | "td" if tables > usize::from(name != "table") => Start::Open(Vec::new(), String::new()),
        "table" => one("table"),
        "tr" => one("tr"),
        "th" => one("th"),
        "td" => one("td"),
        _ if BLOCKS.contains(&name) => one("div"),
        // Every other element: its text, with what its inline style says.
        _ => Start::Open(
            styled(attribute(attributes, "style").as_deref().unwrap_or("")),
            String::new(),
        ),
    }
}

/// The formatting an inline `style` asks for, as tags.
fn styled(style: &str) -> Vec<&'static str> {
    let mut tags = Vec::new();
    for declaration in style.split(';') {
        let Some((property, value)) = declaration.split_once(':') else {
            continue;
        };
        let property = property.trim().to_ascii_lowercase();
        let value = value.trim().to_ascii_lowercase();
        let tag = match property.as_str() {
            "font-weight" => (value.starts_with("bold")
                || value == "bolder"
                || value.parse::<u32>().is_ok_and(|weight| weight >= 600))
            .then_some("b"),
            "font-style" => (value.starts_with("italic") || value.starts_with("oblique")).then_some("i"),
            "text-decoration" | "text-decoration-line" => {
                if value.contains("underline") && !tags.contains(&"u") {
                    tags.push("u");
                }
                value.contains("line-through").then_some("s")
            }
            _ => None,
        };
        if let Some(tag) = tag
            && !tags.contains(&tag)
        {
            tags.push(tag);
        }
    }
    tags
}

fn safe_scheme(href: &str) -> bool {
    let lower = href.to_ascii_lowercase();
    ["http://", "https://", "mailto:", "tel:"]
        .iter()
        .any(|scheme| lower.starts_with(scheme))
}

struct Open {
    source: String,
    outputs: Vec<&'static str>,
}

struct Cleaner {
    out: String,
    open: Vec<Open>,
    formatted: bool,
}

impl Cleaner {
    /// Closes the open elements from `index` up.
    fn close_from(&mut self, index: usize) {
        for element in self.open.drain(index..).rev() {
            for tag in element.outputs.iter().rev() {
                self.out.push_str("</");
                self.out.push_str(tag);
                self.out.push('>');
            }
        }
    }

    fn position(&self, matches: impl Fn(&Open) -> bool) -> Option<usize> {
        self.open.iter().rposition(matches)
    }

    fn opens(element: &Open, tag: &str) -> bool {
        element.outputs.first() == Some(&tag)
    }

    /// End tags HTML leaves out: a new list item closes the open one of its
    /// list, a new row or cell the open one of its table or row, a block an
    /// open paragraph.
    fn implicit_close(&mut self, tag: &str) {
        let scope = |boundaries: &'static [&'static str], target: &'static [&'static str], cleaner: &Self| {
            let boundary = cleaner
                .position(|element| boundaries.iter().any(|b| Self::opens(element, b)))
                .map_or(0, |index| index + 1);
            cleaner.open[boundary..]
                .iter()
                .position(|element| target.iter().any(|t| Self::opens(element, t)))
                .map(|index| boundary + index)
        };
        let close = match tag {
            "li" => scope(&["ul", "ol"], &["li"], self),
            "tr" => scope(&["table"], &["tr"], self),
            "td" | "th" => scope(&["tr", "table"], &["td", "th"], self),
            "p" | "div" | "pre" | "blockquote" | "ul" | "ol" | "table" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
            | "hr" => self
                .open
                .last()
                .filter(|last| Self::opens(last, "p"))
                .map(|_| self.open.len() - 1),
            _ => None,
        };
        if let Some(index) = close {
            self.close_from(index);
        }
    }
}

/// Cleans `html` to the markup described in the module notes: balanced,
/// with text kept as written (stray `<` and `&` escaped).
pub fn sanitize_html(html: &str) -> Sanitized {
    let mut cleaner = Cleaner {
        out: String::with_capacity(html.len().min(1 << 20)),
        open: Vec::new(),
        formatted: false,
    };
    let mut rest = html;
    while let Some(at) = rest.find(['<', '&']) {
        cleaner.out.push_str(&rest[..at]);
        rest = &rest[at..];
        if rest.starts_with('&') {
            let entity = rest[1..].find(';').filter(|&end| {
                end > 0 && end <= 32 && rest[1..1 + end].bytes().all(|b| b.is_ascii_alphanumeric() || b == b'#')
            });
            match entity {
                Some(end) => {
                    cleaner.out.push_str(&rest[..end + 2]);
                    rest = &rest[end + 2..];
                }
                None => {
                    cleaner.out.push_str("&amp;");
                    rest = &rest[1..];
                }
            }
            continue;
        }
        // A comment, doctype or processing instruction: dropped.
        if let Some(comment) = rest.strip_prefix("<!--") {
            rest = comment.find("-->").map_or("", |end| &comment[end + 3..]);
            continue;
        }
        if rest.starts_with("<!") || rest.starts_with("<?") {
            rest = rest.find('>').map_or("", |end| &rest[end + 1..]);
            continue;
        }
        let closing = rest[1..].starts_with('/');
        let name_start = if closing { 2 } else { 1 };
        let name_len = rest[name_start..]
            .bytes()
            .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b':')
            .count();
        if name_len == 0 || !rest.as_bytes()[name_start].is_ascii_alphabetic() {
            // Not a tag: a literal '<'.
            cleaner.out.push_str("&lt;");
            rest = &rest[1..];
            continue;
        }
        let name = rest[name_start..name_start + name_len].to_ascii_lowercase();
        let Some(end) = tag_end(&rest[name_start + name_len..]) else {
            // An unterminated tag: the rest is not text either.
            rest = "";
            break;
        };
        let attributes = &rest[name_start + name_len..name_start + name_len + end];
        rest = &rest[name_start + name_len + end + 1..];
        if closing {
            if let Some(index) = cleaner.position(|element| element.source == name) {
                cleaner.close_from(index);
            }
            continue;
        }
        let tables = cleaner
            .open
            .iter()
            .filter(|element| Cleaner::opens(element, "table"))
            .count()
            + cleaner
                .open
                .iter()
                .filter(|element| element.source == "table" && element.outputs.is_empty())
                .count();
        match start(&name, attributes, tables) {
            // A self-closed one (`<svg ... />`) has no content to skip.
            Start::Drop if attributes.trim_end().ends_with('/') => {}
            Start::Drop => rest = skip_element(rest, &name),
            Start::Void(markup) => {
                if name == "hr" {
                    cleaner.implicit_close("hr");
                }
                if !markup.is_empty() && name != "br" {
                    cleaner.formatted = true;
                }
                cleaner.out.push_str(&markup);
            }
            Start::Open(outputs, first_attributes) => {
                if let Some(first) = outputs.first() {
                    cleaner.implicit_close(first);
                }
                for (index, tag) in outputs.iter().enumerate() {
                    if !matches!(*tag, "p" | "div") {
                        cleaner.formatted = true;
                    }
                    cleaner.out.push('<');
                    cleaner.out.push_str(tag);
                    if index == 0 {
                        cleaner.out.push_str(&first_attributes);
                    }
                    cleaner.out.push('>');
                }
                cleaner.open.push(Open { source: name, outputs });
            }
        }
    }
    cleaner.out.push_str(rest);
    cleaner.close_from(0);
    Sanitized {
        html: cleaner.out,
        formatted: cleaner.formatted,
    }
}

/// Offset of the `>` ending a tag whose name was just read, outside quoted
/// attribute values.
fn tag_end(tail: &str) -> Option<usize> {
    let mut quote = None;
    for (index, byte) in tail.bytes().enumerate() {
        match (quote, byte) {
            (Some(q), b) if b == q => quote = None,
            (Some(_), _) => {}
            (None, b'"' | b'\'') => quote = Some(byte),
            (None, b'>') => return Some(index),
            _ => {}
        }
    }
    None
}

/// The text after the end tag of dropped element `name` (case-insensitive);
/// nothing when it is never closed.
fn skip_element<'a>(rest: &'a str, name: &str) -> &'a str {
    let lower = rest.to_ascii_lowercase();
    let close = format!("</{name}");
    let mut from = 0;
    while let Some(found) = lower[from..].find(&close) {
        let at = from + found;
        let after = at + close.len();
        if lower[after..].starts_with(|c: char| c == '>' || c.is_ascii_whitespace() || c == '/') {
            return rest[after..].find('>').map_or("", |end| &rest[after + end + 1..]);
        }
        from = after;
    }
    ""
}

/// The value of attribute `name` (character references decoded).
fn attribute(attributes: &str, name: &str) -> Option<String> {
    let lower = attributes.to_ascii_lowercase();
    let bytes = lower.as_bytes();
    let mut from = 0;
    loop {
        let at = from + lower[from..].find(name)?;
        from = at + name.len();
        let before = at == 0 || bytes[at - 1].is_ascii_whitespace() || bytes[at - 1] == b'/';
        let after = lower[from..].trim_start();
        if !before || !after.starts_with('=') {
            continue;
        }
        // `lower` and `attributes` have the same byte offsets (ASCII case only).
        let raw = attributes[attributes.len() - after.len() + 1..].trim_start();
        let value = match raw.chars().next()? {
            quote @ ('"' | '\'') => raw[1..].split(quote).next()?,
            _ => raw.split(|c: char| c.is_ascii_whitespace() || c == '>').next()?,
        };
        return Some(decode_entities(value));
    }
}

/// The few character references an attribute value uses in practice.
fn decode_entities(value: &str) -> String {
    value
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn push_escaped(out: &mut String, text: &str, attribute: bool) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' if attribute => out.push_str("&quot;"),
            c => out.push(c),
        }
    }
}

/// RTF destinations whose content is not document text.
const SKIPPED_DESTINATIONS: &[&str] = &[
    "fonttbl",
    "colortbl",
    "stylesheet",
    "info",
    "pict",
    "object",
    "header",
    "headerl",
    "headerr",
    "headerf",
    "footer",
    "footerl",
    "footerr",
    "footerf",
    "footnote",
    "listtable",
    "listoverridetable",
    "rsidtbl",
    "generator",
    "themedata",
    "colorschememapping",
    "datastore",
    "latentstyles",
    "pgdsctbl",
    "xmlnstbl",
    "filetbl",
    "revtbl",
    "nonshppict",
    "shp",
    "shpinst",
    "bkmkstart",
    "bkmkend",
    "expandedcolortbl",
];

#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct CharFormat {
    bold: bool,
    italic: bool,
    underline: bool,
    strike: bool,
}

#[derive(Clone)]
struct Group {
    format: CharFormat,
    skip: bool,
    /// Fallback characters to skip after `\u` (`\uc`).
    uc: usize,
    /// Inside `\fldinst`: its text names the field.
    instruction: bool,
    /// Inside `\fldrslt` of a HYPERLINK field opened as `<a>` here.
    link: bool,
}

impl Default for Group {
    fn default() -> Self {
        Self {
            format: CharFormat::default(),
            skip: false,
            uc: 1,
            instruction: false,
            link: false,
        }
    }
}

struct Writer {
    out: String,
    /// Character formatting of the open inline tags.
    emitted: CharFormat,
    /// A HYPERLINK field's target waiting for its result.
    field: Option<String>,
    instruction: String,
    /// Pending UTF-16 high surrogate.
    high: Option<u16>,
}

impl Writer {
    fn close_inline(&mut self) {
        let f = self.emitted;
        for (on, tag) in [(f.strike, "s"), (f.underline, "u"), (f.italic, "i"), (f.bold, "b")] {
            if on {
                self.out.push_str("</");
                self.out.push_str(tag);
                self.out.push('>');
            }
        }
        self.emitted = CharFormat::default();
    }

    fn text(&mut self, format: CharFormat, text: &str) {
        if format != self.emitted {
            self.close_inline();
            for (on, tag) in [
                (format.bold, "b"),
                (format.italic, "i"),
                (format.underline, "u"),
                (format.strike, "s"),
            ] {
                if on {
                    self.out.push('<');
                    self.out.push_str(tag);
                    self.out.push('>');
                }
            }
            self.emitted = format;
        }
        push_escaped(&mut self.out, text, false);
    }

    fn markup(&mut self, markup: &str) {
        self.out.push_str(markup);
    }
}

/// Converts RTF to the sanitizer's markup (see the module notes).
pub fn rtf_to_html(rtf: &str) -> String {
    let bytes = rtf.as_bytes();
    let mut stack: Vec<Group> = Vec::new();
    let mut group = Group::default();
    let mut writer = Writer {
        out: String::with_capacity(rtf.len() / 2),
        emitted: CharFormat::default(),
        field: None,
        instruction: String::new(),
        high: None,
    };
    let mut pending_skip = 0usize;
    let mut destination_next = false;
    let mut index = 0;
    // Emits one decoded character, honouring the `\uc` fallback skip.
    fn emit(writer: &mut Writer, group: &Group, pending_skip: &mut usize, c: char) {
        if *pending_skip > 0 {
            *pending_skip -= 1;
            return;
        }
        if group.skip {
            return;
        }
        if group.instruction {
            writer.instruction.push(c);
            return;
        }
        let mut buffer = [0; 4];
        writer.text(group.format, c.encode_utf8(&mut buffer));
    }
    while index < bytes.len() {
        let byte = bytes[index];
        match byte {
            b'{' => {
                stack.push(group.clone());
                group.link = false;
                index += 1;
            }
            b'}' => {
                let parent = stack.pop().unwrap_or_default();
                // The whole instruction (it may span nested groups) names
                // the field once its outermost group ends.
                if group.instruction && !parent.instruction {
                    let instruction = std::mem::take(&mut writer.instruction);
                    writer.field = hyperlink_target(&instruction);
                }
                if group.link {
                    writer.close_inline();
                    writer.markup("</a>");
                }
                group = parent;
                index += 1;
            }
            b'\\' => {
                index += 1;
                let Some(&next) = bytes.get(index) else { break };
                if next.is_ascii_alphabetic() {
                    let start = index;
                    while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
                        index += 1;
                    }
                    let word = &rtf[start..index];
                    let number_start = index;
                    if index < bytes.len() && bytes[index] == b'-' {
                        index += 1;
                    }
                    while index < bytes.len() && bytes[index].is_ascii_digit() {
                        index += 1;
                    }
                    let parameter: Option<i64> = rtf[number_start..index].parse().ok();
                    if index < bytes.len() && bytes[index] == b' ' {
                        index += 1;
                    }
                    if destination_next || word == "fldinst" || SKIPPED_DESTINATIONS.contains(&word) {
                        destination_next = false;
                        if word == "fldinst" {
                            group.instruction = true;
                        } else {
                            group.skip = true;
                        }
                        continue;
                    }
                    let on = parameter != Some(0);
                    match word {
                        "b" => group.format.bold = on,
                        "i" => group.format.italic = on,
                        "ul" => group.format.underline = on,
                        "ulnone" => group.format.underline = false,
                        "strike" | "striked" => group.format.strike = on,
                        "plain" => group.format = CharFormat::default(),
                        "uc" => group.uc = parameter.unwrap_or(1).clamp(0, 16) as usize,
                        "par" | "line" | "sect" | "page" if !group.skip && !group.instruction => {
                            writer.close_inline();
                            writer.markup("<br>");
                        }
                        "tab" | "cell" => emit(&mut writer, &group, &mut pending_skip, '\t'),
                        "row" if !group.skip => {
                            writer.close_inline();
                            writer.markup("<br>");
                        }
                        "emdash" => emit(&mut writer, &group, &mut pending_skip, '\u{2014}'),
                        "endash" => emit(&mut writer, &group, &mut pending_skip, '\u{2013}'),
                        "bullet" => emit(&mut writer, &group, &mut pending_skip, '\u{2022}'),
                        "lquote" => emit(&mut writer, &group, &mut pending_skip, '\u{2018}'),
                        "rquote" => emit(&mut writer, &group, &mut pending_skip, '\u{2019}'),
                        "ldblquote" => emit(&mut writer, &group, &mut pending_skip, '\u{201c}'),
                        "rdblquote" => emit(&mut writer, &group, &mut pending_skip, '\u{201d}'),
                        "u" => {
                            if let Some(value) = parameter {
                                let unit = if value < 0 {
                                    (value + 65536) as u16
                                } else {
                                    value as u16
                                };
                                let decoded = match (writer.high.take(), unit) {
                                    (_, 0xd800..=0xdbff) => {
                                        writer.high = Some(unit);
                                        None
                                    }
                                    (Some(high), 0xdc00..=0xdfff) => {
                                        char::decode_utf16([high, unit]).next().and_then(Result::ok)
                                    }
                                    (_, unit) => char::from_u32(u32::from(unit)),
                                };
                                if let Some(c) = decoded {
                                    emit(&mut writer, &group, &mut 0, c);
                                }
                                pending_skip = group.uc;
                            }
                        }
                        "fldrslt" => {
                            if let Some(target) = writer.field.take()
                                && !group.skip
                            {
                                writer.close_inline();
                                let mut anchor = String::from("<a href=\"");
                                push_escaped(&mut anchor, &target, true);
                                anchor.push_str("\">");
                                writer.markup(&anchor);
                                group.link = true;
                            }
                        }
                        "bin" => {
                            index = index.saturating_add(parameter.unwrap_or(0).max(0) as usize);
                        }
                        _ => {}
                    }
                } else {
                    index += 1;
                    match next {
                        b'*' => destination_next = true,
                        b'\'' => {
                            let hex = rtf
                                .get(index..index + 2)
                                .and_then(|hex| u8::from_str_radix(hex, 16).ok());
                            index += 2;
                            if let Some(value) = hex {
                                emit(&mut writer, &group, &mut pending_skip, windows_1252(value));
                            }
                        }
                        b'~' => emit(&mut writer, &group, &mut pending_skip, '\u{a0}'),
                        b'_' => emit(&mut writer, &group, &mut pending_skip, '\u{2011}'),
                        b'\\' | b'{' | b'}' => emit(&mut writer, &group, &mut pending_skip, next as char),
                        b'\n' | b'\r' if !group.skip => {
                            writer.close_inline();
                            writer.markup("<br>");
                        }
                        _ => {}
                    }
                }
            }
            b'\r' | b'\n' => index += 1,
            _ => {
                // Plain text runs to the next special byte.
                let start = index;
                while index < bytes.len() && !matches!(bytes[index], b'{' | b'}' | b'\\' | b'\r' | b'\n') {
                    index += 1;
                }
                for c in rtf[start..index].chars() {
                    emit(&mut writer, &group, &mut pending_skip, c);
                }
            }
        }
    }
    writer.close_inline();
    let mut out = writer.out;
    while out.ends_with("<br>") {
        out.truncate(out.len() - 4);
    }
    out
}

/// The target of a `HYPERLINK "url"` field instruction with a safe scheme.
fn hyperlink_target(instruction: &str) -> Option<String> {
    let rest = instruction.trim_start().strip_prefix("HYPERLINK")?.trim_start();
    let target = match rest.strip_prefix('"') {
        Some(quoted) => quoted.split('"').next()?,
        None => rest.split_whitespace().next()?,
    };
    safe_scheme(target).then(|| target.to_owned())
}

/// A `\'hh` byte as Windows-1252 (Latin-1 outside 0x80–0x9F). Other code
/// pages are not decoded (see the module notes).
fn windows_1252(byte: u8) -> char {
    const HIGH: [char; 32] = [
        '\u{20ac}', '\u{fffd}', '\u{201a}', '\u{0192}', '\u{201e}', '\u{2026}', '\u{2020}', '\u{2021}', '\u{02c6}',
        '\u{2030}', '\u{0160}', '\u{2039}', '\u{0152}', '\u{fffd}', '\u{017d}', '\u{fffd}', '\u{fffd}', '\u{2018}',
        '\u{2019}', '\u{201c}', '\u{201d}', '\u{2022}', '\u{2013}', '\u{2014}', '\u{02dc}', '\u{2122}', '\u{0161}',
        '\u{203a}', '\u{0153}', '\u{fffd}', '\u{017e}', '\u{0178}',
    ];
    match byte {
        0x80..=0x9f => HIGH[usize::from(byte - 0x80)],
        byte => char::from(byte),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clean(html: &str) -> String {
        sanitize_html(html).html
    }

    #[test]
    fn cleaning_keeps_basic_structure_and_drops_everything_active() {
        let html = r#"<html><head><style>p{color:red}</style><script>alert(1)</script></head>
<body><!--StartFragment--><h2 class="x">Title</h2><p style="color:red" onclick="x()">Hello <strong>bold</strong> &amp; <em>it</em></p>
<ul><li>one</li><li>two</li></ul><a href="https://example.com/?a=1&amp;b=2" target="_blank">link</a>
<a href="javascript:alert(1)">bad</a><img src="https://tracker.example/x.png" alt="pic"><iframe src="https://x"></iframe>
<form><input value="secret"><button>go</button></form><SCRIPT type="text/javascript">evil()</SCRIPT>1 < 2<!--EndFragment--></body></html>"#;
        let clean = sanitize_html(html);
        assert!(clean.formatted);
        let html = clean.html;
        assert!(html.contains("<h2>Title</h2>"), "{html}");
        assert!(html.contains("<p>Hello <b>bold</b> &amp; <i>it</i></p>"), "{html}");
        assert!(html.contains("<ul><li>one</li><li>two</li></ul>"), "{html}");
        assert!(
            html.contains(r#"<a href="https://example.com/?a=1&amp;b=2">link</a>"#),
            "{html}"
        );
        assert!(html.contains("\nbad<img alt=\"pic\">"), "{html}");
        assert!(html.contains("1 &lt; 2"), "{html}");
        for gone in [
            "script", "alert", "evil", "style", "color", "onclick", "src", "tracker", "iframe", "class", "secret",
            "go", "target",
        ] {
            assert!(!html.to_ascii_lowercase().contains(gone), "{gone} in {html}");
        }
    }

    #[test]
    fn tables_lists_quotes_code_and_inline_styles_are_kept_normalized() {
        assert_eq!(
            clean("<table><tbody><tr><th>a</th><th>b</th></tr><tr><td>1<td>2</tbody></table>"),
            "<table><tr><th>a</th><th>b</th></tr><tr><td>1</td><td>2</td></tr></table>"
        );
        // A table inside a cell keeps only its text.
        assert_eq!(
            clean("<table><tr><td><table><tr><td>in</td></tr></table></td></tr></table>"),
            "<table><tr><td>in</td></tr></table>"
        );
        assert_eq!(
            clean(r#"<ol start="3" type="a"><li>x<li>y</ol><ul><li>a<ul><li>b</ul><li>c</ul>"#),
            r#"<ol start="3"><li>x</li><li>y</li></ol><ul><li>a<ul><li>b</li></ul></li><li>c</li></ul>"#
        );
        assert_eq!(
            clean("<blockquote>q</blockquote><pre>a  b\n c</pre><kbd>k</kbd><hr><del>d</del><ins>n</ins>"),
            "<blockquote>q</blockquote><pre>a  b\n c</pre><code>k</code><hr><s>d</s><u>n</u>"
        );
        assert_eq!(
            clean(
                r#"<span style="font-weight: 700; font-style:italic">bi</span><font style="text-decoration: underline line-through">ul</font><span style="color:red">red</span>"#
            ),
            "<b><i>bi</i></b><u><s>ul</s></u>red"
        );
        assert_eq!(
            clean(r#"<section><div>a</div><p>b<p>c</section><a href="tel:+123">call</a><a href="mailto:x@y">mail</a>"#),
            r#"<div><div>a</div><p>b</p><p>c</p></div><a href="tel:+123">call</a><a href="mailto:x@y">mail</a>"#
        );
    }

    #[test]
    fn markup_without_formatting_is_plain_text() {
        for plain in [
            r#"<p style="color:red;font-size:20px">just colour</p>"#,
            "<div><span>text</span><br><font face=Arial>font</font></div>",
            r#"<a href="javascript:x()">not a link</a>"#,
            "no markup at all",
        ] {
            assert!(!sanitize_html(plain).formatted, "{plain}");
        }
        assert!(sanitize_html("<b>x</b>").formatted);
    }

    #[test]
    fn cleaned_markup_is_balanced() {
        assert_eq!(clean("<b><i>x</b> y"), "<b><i>x</i></b> y");
        assert_eq!(clean("</p>text<p>open"), "text<p>open</p>");
        assert_eq!(
            clean("<div>unterminated <a href=\"https://x"),
            "<div>unterminated </div>"
        );
    }

    #[test]
    fn rtf_converts_formatting_paragraphs_unicode_and_links() {
        let rtf = r#"{\rtf1\ansi\ansicpg1252\cocoartf2761
{\fonttbl\f0\fswiss\fcharset0 Helvetica;}
{\colortbl;\red255\green255\blue255;}
{\*\expandedcolortbl;;}
\pard\f0\fs24 \cf0 Plain \b bold\b0  and \i italic\i0  \ul under\ulnone .\
Caf\'e9 \uc0\u20320 \u22909  {\field{\*\fldinst{HYPERLINK "https://example.com/a?b=1&c=2"}}{\fldrslt link}}\par
{\field{\*\fldinst{HYPERLINK "javascript:x()"}}{\fldrslt plain}} 5 < 6}"#;
        let html = rtf_to_html(rtf);
        assert_eq!(
            html,
            "Plain <b>bold</b> and <i>italic</i> <u>under</u>.<br>Café 你好 <a href=\"https://example.com/a?b=1&amp;c=2\">link</a><br>plain 5 &lt; 6"
        );
        assert!(!html.contains("Helvetica"));
        // The converted markup survives cleaning unchanged.
        assert_eq!(clean(&html), html);
        // RTF with only fonts and colours is plain text.
        assert!(!sanitize_html(&rtf_to_html(r"{\rtf1{\fonttbl\f0 Arial;}\f0\cf1 only colour}")).formatted);
    }

    #[test]
    fn rtf_unicode_fallbacks_and_surrogates() {
        assert_eq!(rtf_to_html(r"{\rtf1 a\u8364?b}"), "a\u{20ac}b");
        assert_eq!(rtf_to_html(r"{\rtf1\uc2 \u8364??c}"), "\u{20ac}c");
        assert_eq!(rtf_to_html(r"{\rtf1 \u-10179?\u-8704?}"), "\u{1f600}");
    }

    #[test]
    fn clipboard_rich_text_needs_formatting() {
        assert_eq!(
            ClipboardRich::from_source(RichTextFormat::Html, "<b>x</b>".into()),
            Some(ClipboardRich {
                html: "<b>x</b>".into(),
                rtf: None
            })
        );
        assert_eq!(
            ClipboardRich::from_source(RichTextFormat::Html, "<span>x</span>".into()),
            None
        );
        let rtf = r"{\rtf1 \b x\b0}".to_owned();
        assert_eq!(
            ClipboardRich::from_source(RichTextFormat::Rtf, rtf.clone()),
            Some(ClipboardRich {
                html: "<b>x</b>".into(),
                rtf: Some(rtf)
            })
        );
    }
}
