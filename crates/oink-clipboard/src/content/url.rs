//! `oink:url/1`: bounded UTF-8 JSON {"url": string, "title": string|null}.
//! Native representations stay raw on same-system transfers. Companion titles
//! are read only on demand, from the same item and under the source's guards.
use super::{Provider, guarded, unavailable};
use crate::model::{
    ContentItem, Inventory, NativeFormat, Representation, RepresentationData, StandardFormat, Unavailable,
};
use oink_foundation::clipboard::ClipPlatform;
use serde::{Deserialize, Serialize};
use std::io::{self, Read};

pub(crate) const MAX_URL_BYTES: usize = 16 * 1024;
const MAX_TITLE_BYTES: usize = 4 * 1024;
const MAX_SOURCE_BYTES: usize = 64 * 1024;
pub(crate) const MAX_WIRE_BYTES: usize = 128 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Link {
    pub url: String,
    pub title: Option<String>,
}
fn invalid() -> io::Error {
    unavailable(Unavailable::InvalidData)
}
fn validate_url(url: &str) -> io::Result<()> {
    let (scheme, body) = url.split_once(':').ok_or_else(invalid)?;
    if url.len() > MAX_URL_BYTES
        || body.is_empty()
        || scheme.is_empty()
        || !scheme.as_bytes()[0].is_ascii_alphabetic()
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
        || url.chars().any(|c| c.is_whitespace() || c.is_control())
        || (matches!(scheme.to_ascii_lowercase().as_str(), "http" | "https" | "ftp")
            && !body
                .strip_prefix("//")
                .is_some_and(|s| !s.split(['/', '?', '#']).next().unwrap_or_default().is_empty()))
    {
        return Err(invalid());
    }
    Ok(())
}
fn title(text: String) -> Option<String> {
    (!text.is_empty() && text.len() <= MAX_TITLE_BYTES && !text.chars().any(char::is_control)).then_some(text)
}
pub(crate) fn read(reader: &mut dyn Read) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader.take(MAX_SOURCE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(unavailable(Unavailable::LimitExceeded));
    }
    Ok(bytes)
}
fn wide(bytes: &[u8]) -> io::Result<String> {
    // HGLOBAL may have allocation padding after the first native terminator.
    let words: Vec<_> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .take_while(|w| *w != 0)
        .collect();
    if !bytes.len().is_multiple_of(2) && words.len() * 2 == bytes.len() - 1 {
        return Err(invalid());
    }
    String::from_utf16(&words).map_err(|_| invalid())
}
fn utf8(bytes: &[u8]) -> io::Result<String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| invalid())
}
fn native(format: &NativeFormat, bytes: &[u8]) -> io::Result<Link> {
    if bytes.len() > MAX_SOURCE_BYTES {
        return Err(unavailable(Unavailable::LimitExceeded));
    }
    let (url, name) = match format {
        NativeFormat::Mac(name) if name == "public.url" => (utf8(bytes)?, name.as_str()),
        NativeFormat::WindowsRegistered(name)
            if name.eq_ignore_ascii_case("UniformResourceLocatorW") || name.eq_ignore_ascii_case("text/x-moz-url") =>
        {
            (wide(bytes)?, name.as_str())
        }
        NativeFormat::WindowsRegistered(name) if name.eq_ignore_ascii_case("UniformResourceLocator") => {
            let bytes = &bytes[..bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len())];
            #[cfg(windows)]
            let text = crate::windows::decode_codepage(bytes, windows::Win32::Globalization::CP_ACP)?;
            #[cfg(not(windows))]
            let text = utf8(bytes)?;
            (text, name.as_str())
        }
        _ => return Err(unavailable(Unavailable::Unsupported)),
    };
    let (url, title) = if name.eq_ignore_ascii_case("text/x-moz-url") {
        let (url, name) = url.split_once('\n').ok_or_else(invalid)?;
        let name = name.trim_end_matches(['\r', '\n']);
        if name.contains(['\r', '\n']) {
            return Err(invalid());
        }
        (url.trim_end_matches('\r').to_owned(), title(name.to_owned()))
    } else {
        (url, None)
    };
    validate_url(&url)?;
    Ok(Link { url, title })
}
pub(crate) fn title_format(format: &NativeFormat) -> bool {
    matches!(format, NativeFormat::Mac(name) if name == "public.url-name")
        || matches!(format, NativeFormat::WindowsRegistered(name) if name.eq_ignore_ascii_case("text/x-moz-url"))
}
/// Native publication prefers Unicode URLs, while every independently
/// convertible URL representation remains available to demand and transfer.
#[cfg(target_os = "macos")]
pub(crate) fn priority(rep: &Representation) -> u8 {
    match &rep.format {
        NativeFormat::WindowsRegistered(name) if name.eq_ignore_ascii_case("UniformResourceLocator") => 1,
        NativeFormat::WindowsRegistered(name) if name.eq_ignore_ascii_case("text/x-moz-url") => 2,
        _ => 0,
    }
}
pub(crate) fn capture(
    provider: &dyn Provider,
    inventory: &Inventory,
    rep: &Representation,
    bytes: &[u8],
    hover: bool,
) -> io::Result<Link> {
    let mut link = native(&rep.format, bytes)?;
    let item = inventory
        .items
        .iter()
        .find(|item| item.representations.iter().any(|r| r.id == rep.id))
        .ok_or_else(invalid)?;
    for companion in item
        .representations
        .iter()
        .filter(|r| r.id != rep.id && title_format(&r.format))
    {
        let result = (|| {
            let reader = match &companion.data {
                RepresentationData::Inline(bytes) => Box::new(io::Cursor::new(bytes.clone())) as crate::FileReader,
                RepresentationData::Reader { .. } if hover => provider.open_hover(companion.id)?,
                RepresentationData::Reader { .. } => provider.open(companion.id)?,
                _ => return Err(invalid()),
            };
            let mut reader = guarded(reader, provider.reader_guard(companion.id)?)?;
            let bytes = read(&mut reader)?;
            match &companion.format {
                NativeFormat::Mac(_) => Ok(title(utf8(&bytes)?)),
                _ => {
                    let moz = native(&companion.format, &bytes)?;
                    Ok((moz.url == link.url).then_some(moz.title).flatten())
                }
            }
        })();
        match result {
            Ok(Some(name)) => {
                link.title = Some(name);
                break;
            }
            Err(error)
                if matches!(
                    super::reason(&error),
                    Unavailable::Superseded | Unavailable::Permission | Unavailable::Cancelled
                ) =>
            {
                return Err(error);
            }
            _ => {}
        }
    }
    Ok(link)
}
/// Titles are advertised only when a native title source was offered. Actual
/// bytes remain lazy; a source offering an empty/invalid title supplies no data.
pub(crate) fn names(item: &ContentItem, target: ClipPlatform) -> Vec<String> {
    let has_title = item
        .representations
        .iter()
        .any(|r| title_format(&r.format) && !matches!(r.data, RepresentationData::Unavailable(_)));
    let has_text = item
        .representations
        .iter()
        .any(|r| r.standard == Some(StandardFormat::Text) && !matches!(r.data, RepresentationData::Unavailable(_)));
    let has_html = item
        .representations
        .iter()
        .any(|r| r.standard == Some(StandardFormat::Html));
    let mut names = match target {
        ClipPlatform::Mac => vec!["public.url"],
        ClipPlatform::Windows => vec!["UniformResourceLocatorW", "UniformResourceLocator"],
        ClipPlatform::Unknown => return Vec::new(),
    };
    if has_title {
        names.push(if target == ClipPlatform::Mac {
            "public.url-name"
        } else {
            "text/x-moz-url"
        });
    }
    if has_title && !has_html {
        names.push(if target == ClipPlatform::Mac {
            "public.html"
        } else {
            "HTML Format"
        });
    }
    if !has_text {
        names.push(if target == ClipPlatform::Mac {
            "public.utf8-plain-text"
        } else {
            "CF_UNICODETEXT"
        });
    }
    names.into_iter().map(str::to_owned).collect()
}
impl Link {
    fn anchor(&self) -> io::Result<String> {
        fn escape(value: &str) -> String {
            value
                .replace('&', "&amp;")
                .replace('<', "&lt;")
                .replace('>', "&gt;")
                .replace('"', "&quot;")
                .replace('\'', "&#39;")
        }
        let title = self.title.as_ref().ok_or_else(invalid)?;
        Ok(format!("<a href=\"{}\">{}</a>", escape(&self.url), escape(title)))
    }

    pub(crate) fn encode(&self) -> io::Result<Vec<u8>> {
        self.validate()?;
        serde_json::to_vec(self).map_err(|_| invalid())
    }
    pub(crate) fn decode(bytes: &[u8]) -> io::Result<Self> {
        if bytes.len() > MAX_WIRE_BYTES {
            return Err(unavailable(Unavailable::LimitExceeded));
        }
        let link: Self = serde_json::from_slice(bytes).map_err(|_| invalid())?;
        link.validate()?;
        Ok(link)
    }
    fn validate(&self) -> io::Result<()> {
        validate_url(&self.url)?;
        if self.title.as_ref().is_some_and(|s| title(s.clone()).is_none()) {
            return Err(invalid());
        }
        Ok(())
    }
    pub(crate) fn render(&self, target: ClipPlatform, name: &str) -> io::Result<Vec<u8>> {
        self.validate()?;
        match (target, name) {
            (ClipPlatform::Mac, "public.url" | "public.utf8-plain-text") => Ok(self.url.as_bytes().to_vec()),
            (ClipPlatform::Mac, "public.url-name") => {
                self.title.as_ref().map(|s| s.as_bytes().to_vec()).ok_or_else(invalid)
            }
            (ClipPlatform::Mac, "public.html") => Ok(self.anchor()?.into_bytes()),
            #[cfg(windows)]
            (ClipPlatform::Windows, "HTML Format") => Ok(crate::windows::html::encode_cf_html(&self.anchor()?)),
            (ClipPlatform::Windows, "UniformResourceLocatorW" | "CF_UNICODETEXT") => Ok(to_wide(&self.url)),
            (ClipPlatform::Windows, "UniformResourceLocator") => ansi(&self.url),
            (ClipPlatform::Windows, "text/x-moz-url") => self
                .title
                .as_ref()
                .map(|s| to_wide(&format!("{}\n{}", self.url, s)))
                .ok_or_else(invalid),
            _ => Err(unavailable(Unavailable::Unsupported)),
        }
    }
}
fn to_wide(text: &str) -> Vec<u8> {
    text.encode_utf16().chain(Some(0)).flat_map(u16::to_le_bytes).collect()
}
pub(crate) fn ansi(text: &str) -> io::Result<Vec<u8>> {
    #[cfg(windows)]
    {
        use windows::Win32::Globalization::{CP_ACP, GetACP, WC_NO_BEST_FIT_CHARS, WideCharToMultiByte};
        // UTF-8 ACP rejects WC_NO_BEST_FIT_CHARS and default-character output.
        // SAFETY: GetACP only reads the process system code page.
        if unsafe { GetACP() } == 65001 {
            return Ok(text.bytes().chain(Some(0)).collect());
        }
        use windows::core::BOOL;
        let wide: Vec<_> = text.encode_utf16().chain(Some(0)).collect();
        let mut used = BOOL(0);
        // SAFETY: bounded UTF-16 input; query size, then write that exact buffer.
        let n = unsafe { WideCharToMultiByte(CP_ACP, WC_NO_BEST_FIT_CHARS, &wide, None, None, Some(&mut used)) };
        let mut bytes = vec![0; n.max(0) as usize];
        // SAFETY: the size query allocated exactly n output bytes above.
        let written = unsafe {
            WideCharToMultiByte(
                CP_ACP,
                WC_NO_BEST_FIT_CHARS,
                &wide,
                Some(&mut bytes),
                None,
                Some(&mut used),
            )
        };
        if written == 0 || used.as_bool() {
            return Err(invalid());
        }
        Ok(bytes)
    }
    #[cfg(not(windows))]
    {
        if !text.is_ascii() {
            return Err(invalid());
        }
        Ok(text.bytes().chain(Some(0)).collect())
    }
}

#[cfg(all(test, debug_assertions))]
pub(crate) mod tests {
    use super::*;
    use crate::content::{self, Translator};
    use crate::model::*;
    use std::{collections::HashMap, sync::Arc};
    pub(crate) struct Memory {
        pub inventory: Inventory,
        bytes: HashMap<RepresentationId, Vec<u8>>,
    }
    impl Provider for Memory {
        fn inventory(&self) -> Inventory {
            self.inventory.clone()
        }
        fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
            Ok(Box::new(io::Cursor::new(
                self.bytes.get(&id).ok_or_else(invalid)?.clone(),
            )))
        }
        fn translator(&self) -> Arc<dyn Translator> {
            struct Text;
            impl Translator for Text {
                fn standard(&self, rep: &Representation, bytes: &[u8]) -> io::Result<Vec<crate::ClipData>> {
                    assert_eq!(rep.standard, Some(StandardFormat::Text));
                    let text = if rep.format == NativeFormat::WindowsPredefined(13) {
                        wide(bytes)?
                    } else {
                        utf8(bytes)?
                    };
                    Ok(vec![crate::ClipData::Text(text)])
                }
            }
            Arc::new(Text)
        }
    }
    pub(crate) fn source(platform: ClipPlatform, entries: &[(&str, Vec<u8>)]) -> Memory {
        let mut inventory = crate::content::tests::fixture().inventory();
        inventory.platform = platform;
        inventory.items[0].representations = entries
            .iter()
            .enumerate()
            .map(|(i, (name, bytes))| Representation {
                id: RepresentationId(i as u32 + 1),
                format: if *name == "CF_UNICODETEXT" {
                    NativeFormat::WindowsPredefined(13)
                } else if platform == ClipPlatform::Mac {
                    NativeFormat::Mac((*name).into())
                } else {
                    NativeFormat::WindowsRegistered((*name).into())
                },
                medium: if platform == ClipPlatform::Mac {
                    Medium::Bytes
                } else {
                    Medium::HGlobal
                },
                aspect: 1,
                index: -1,
                standard: match *name {
                    "public.url" | "UniformResourceLocatorW" | "UniformResourceLocator" | "text/x-moz-url" => {
                        Some(StandardFormat::Url)
                    }
                    "public.utf8-plain-text" | "CF_UNICODETEXT" => Some(StandardFormat::Text),
                    _ => None,
                },
                data: RepresentationData::Inline(bytes.clone()),
            })
            .collect();
        Memory {
            inventory,
            bytes: entries
                .iter()
                .enumerate()
                .map(|(i, (_, bytes))| (RepresentationId(i as u32 + 1), bytes.clone()))
                .collect(),
        }
    }
    pub(crate) fn wide_bytes(s: &str) -> Vec<u8> {
        to_wide(s)
    }
    pub(crate) struct Projected(pub Memory, pub ClipPlatform);
    impl Provider for Projected {
        fn inventory(&self) -> Inventory {
            self.0.inventory()
        }
        fn open(&self, id: RepresentationId) -> io::Result<crate::FileReader> {
            content::open_for(&self.0, id, self.1)
        }
        fn translator(&self) -> Arc<dyn Translator> {
            self.0.translator()
        }
    }
    fn projected(source: &Memory, id: u32, target: ClipPlatform) -> Link {
        let bytes = content::read_all(&mut content::open_for(source, RepresentationId(id), target).unwrap()).unwrap();
        Link::decode(&bytes).unwrap()
    }
    #[test]
    fn approved_anchor_preserves_url_and_escapes_title_without_replacing_html() {
        let mut source = source(
            ClipPlatform::Mac,
            &[
                ("public.url", b"https://example.invalid/?a=1&b=2".to_vec()),
                ("public.url-name", b"<Title> & \"quoted\"".to_vec()),
            ],
        );
        let item = &source.inventory.items[0];
        assert!(names(item, ClipPlatform::Mac).contains(&"public.html".into()));
        assert!(names(item, ClipPlatform::Windows).contains(&"HTML Format".into()));
        let link = projected(&source, 1, ClipPlatform::Windows);
        assert_eq!(
            link.render(ClipPlatform::Mac, "public.html").unwrap(),
            b"<a href=\"https://example.invalid/?a=1&amp;b=2\">&lt;Title&gt; &amp; &quot;quoted&quot;</a>"
        );
        let mut html = source.inventory.items[0].representations[0].clone();
        html.id = RepresentationId(3);
        html.format = NativeFormat::Mac("public.html".into());
        html.standard = Some(StandardFormat::Html);
        source.inventory.items[0].representations.push(html);
        assert!(!names(&source.inventory.items[0], ClipPlatform::Mac).contains(&"public.html".into()));
        assert!(!names(&source.inventory.items[0], ClipPlatform::Windows).contains(&"HTML Format".into()));
    }
    #[test]
    fn approved_anchor_requires_title_and_never_accepts_header_injection() {
        let source = source(
            ClipPlatform::Mac,
            &[("public.url", b"https://example.invalid/".to_vec())],
        );
        assert!(!names(&source.inventory.items[0], ClipPlatform::Mac).contains(&"public.html".into()));
        let invalid_link = Link {
            url: "https://example.invalid/\r\nURL=other".into(),
            title: Some("title".into()),
        };
        assert!(invalid_link.render(ClipPlatform::Mac, "public.html").is_err());
    }
    #[test]
    fn paired_url_wire_bytes_are_exact_and_title_is_item_local() {
        let mut source = source(
            ClipPlatform::Mac,
            &[
                ("public.url", b"https://example.invalid/".to_vec()),
                ("public.url-name", "Title 猫".as_bytes().to_vec()),
            ],
        );
        let link = projected(&source, 1, ClipPlatform::Windows);
        assert_eq!(
            link.encode().unwrap(),
            "{\"url\":\"https://example.invalid/\",\"title\":\"Title 猫\"}".as_bytes()
        );
        assert_eq!(
            link.render(ClipPlatform::Windows, "text/x-moz-url").unwrap(),
            to_wide("https://example.invalid/\nTitle 猫")
        );
        let name = source.inventory.items[0].representations.pop().unwrap();
        let mut other = source.inventory.items[0].clone();
        other.id = ItemId(99);
        other.representations = vec![name];
        source.inventory.items.push(other);
        assert!(projected(&source, 1, ClipPlatform::Windows).title.is_none());
        assert_eq!(
            content::read_all(&mut content::open_for(&source, RepresentationId(1), ClipPlatform::Mac).unwrap())
                .unwrap(),
            b"https://example.invalid/"
        );
    }
    #[test]
    fn windows_prefers_w_and_only_uses_matching_mozilla_title() {
        let mut source = source(
            ClipPlatform::Windows,
            &[
                ("UniformResourceLocator", b"https://wrong.invalid/\0".to_vec()),
                ("text/x-moz-url", to_wide("https://example.invalid/\nBrowser title")),
                ("UniformResourceLocatorW", to_wide("https://example.invalid/")),
            ],
        );
        assert_eq!(
            source
                .inventory
                .wanted(ClipPlatform::Mac)
                .map(|r| r.id)
                .collect::<Vec<_>>(),
            [RepresentationId(1), RepresentationId(2), RepresentationId(3)]
        );
        assert_eq!(
            projected(&source, 3, ClipPlatform::Mac).title.as_deref(),
            Some("Browser title")
        );
        source.inventory.items[0]
            .representations
            .retain(|r| r.id != RepresentationId(3));
        assert!(projected(&source, 1, ClipPlatform::Mac).title.is_none());
        source.inventory.items[0]
            .representations
            .retain(|r| r.id != RepresentationId(1));
        assert_eq!(
            projected(&source, 2, ClipPlatform::Mac).title.as_deref(),
            Some("Browser title")
        );
        let invalid_w = self::source(
            ClipPlatform::Windows,
            &[
                ("UniformResourceLocatorW", to_wide("relative")),
                ("UniformResourceLocator", b"https://example.invalid/\0".to_vec()),
            ],
        );
        assert!(content::open_for(&invalid_w, RepresentationId(1), ClipPlatform::Mac).is_err());
        assert!(content::open_for(&invalid_w, RepresentationId(2), ClipPlatform::Mac).is_ok());
    }
    #[test]
    fn urls_are_single_absolute_and_bounded_without_scheme_allowlist() {
        for url in [
            "https://example.invalid/a#b",
            "mailto:person@example.invalid",
            "file:///tmp/example",
            "custom:opaque",
        ] {
            assert!(validate_url(url).is_ok());
        }
        for url in [
            "relative",
            "//example.invalid",
            "https:",
            "https://",
            "https:///path",
            "https://example.invalid/\nhttps://other.invalid/",
            "https://example.invalid/ space",
            "https://example.invalid/\0",
            "1scheme:a",
        ] {
            assert!(validate_url(url).is_err(), "{url:?}");
        }
        assert!(validate_url(&format!("https://example.invalid/{}", "x".repeat(MAX_URL_BYTES))).is_err());
        assert!(read(&mut io::repeat(1)).is_err());
        assert!(Link::decode(&vec![b' '; MAX_WIRE_BYTES + 1]).is_err());
        assert!(native(&NativeFormat::WindowsRegistered("UniformResourceLocatorW".into()), b"a").is_err());
        assert!(
            native(
                &NativeFormat::WindowsRegistered("UniformResourceLocatorW".into()),
                &[0, 216, 0, 0]
            )
            .is_err()
        );
    }
    #[test]
    fn native_names_preserve_text_and_real_file_companion_rules() {
        for platform in [ClipPlatform::Mac, ClipPlatform::Windows] {
            let mut source = source(
                ClipPlatform::Mac,
                &[("public.url", b"https://example.invalid/".to_vec())],
            );
            assert_eq!(
                names(&source.inventory.items[0], platform).len(),
                if platform == ClipPlatform::Mac { 2 } else { 3 }
            );
            source.inventory.items[0].representations.push(Representation {
                id: RepresentationId(2),
                format: NativeFormat::Mac("public.utf8-plain-text".into()),
                medium: Medium::Bytes,
                aspect: 1,
                index: -1,
                standard: Some(StandardFormat::Text),
                data: RepresentationData::Inline(b"Existing text".to_vec()),
            });
            let projected_names = names(&source.inventory.items[0], platform);
            assert!(
                !projected_names
                    .iter()
                    .any(|n| n == "public.utf8-plain-text" || n == "CF_UNICODETEXT")
            );
            source.inventory.items[0].file = Some(NodeId(7));
            assert_eq!(source.inventory.wanted(ClipPlatform::Windows).count(), 0);
            source.inventory.items[0].virtual_files = true;
            assert_eq!(source.inventory.wanted(ClipPlatform::Windows).count(), 2);
        }
    }
    #[test]
    fn url_budget_accounts_for_title_and_encoding_expansion_in_one_total() {
        let mut source = source(
            ClipPlatform::Mac,
            &[("public.url", b"https://example.invalid/".to_vec())],
        );
        assert!(source.inventory.automatic_pull(ClipPlatform::Windows));
        for i in 2..=129 {
            let mut rep = source.inventory.items[0].representations[0].clone();
            rep.id = RepresentationId(i);
            source.inventory.items[0].representations.push(rep);
        }
        assert!(!source.inventory.automatic_pull(ClipPlatform::Windows));
        assert!(source.inventory.automatic_pull(ClipPlatform::Mac));
    }
}
