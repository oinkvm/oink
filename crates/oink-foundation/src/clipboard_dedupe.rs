//! Content echo detection between clipboard synchronizers (0.4.0 user decision).
//!
//! Several synchronizers can share one clipboard: Apple Universal Clipboard, a
//! vendor "phone link", or two Oink devices that each also run a third-party
//! syncer. Each one treats the others' writes as new copies, so content can
//! circle forever. Oink only compares content: it never marks its own writes
//! for other programs and has no switch for this.
//!
//! Echoes are suppressed only where content is announced: the sending side.
//! Received offers are always written. Each device remembers the identity
//! (content fingerprint) of its current clipboard, whether it copied that
//! content itself or Oink wrote it from an offer (then the offer's digests are
//! the fingerprint: the lazily written clipboard is never read to compare).
//! On a local change, in this order:
//! 1. An Oink clip marker naming a known clip identifies that clip: not
//!    announced (always on; exact). This is what handles Universal Clipboard.
//! 2. With "Clipboard deduplication" (device-local setting, default on):
//!    a *format subset* of the current content is not announced, whatever its
//!    source and however much later. Off: every other change is announced.
//!    Data that only another device can provide (Universal Clipboard promised
//!    data) is never read to compare: it counts as unknown, never a subset.
//!
//! The source (an Oink device, or "External") is kept for diagnostics only.
//!
//! Subset tolerance, per class:
//! - Text: equal digests after newline normalization (CRLF/CR to LF, trailing
//!   newlines removed). Different native text formats are one class. A text
//!   prefix or substring is a different text.
//! - Image: equal decoded-pixel digests and dimensions. When either side has
//!   only dimensions (pixel digest above the cost threshold, or not read), the
//!   dimensions alone decide.
//! - File: equal name and kind; size and modification time must agree when
//!   both are known. File bytes are never read.
//! - HTML, RTF, URL: the old content must carry the same class. A digest or a
//!   size decides when both sides know it; otherwise presence suffices. They
//!   never anchor an echo on their own: some text, image or file must match.
//! - Opaque formats (private application formats, synchronizer tags) are
//!   ignored when the new content has any of the classes above, because
//!   synchronizers add their own tag formats. Content made only of opaque
//!   formats must match by native format name and a known equal digest or size.
use crate::DeviceId;
use std::fmt;

/// Normalized-text digests are computed from at most this many source bytes.
/// Larger text is published without a digest and is never treated as an echo.
pub const TEXT_DIGEST_BYTES: usize = 16 * 1024 * 1024;
/// Encoded image bytes read for a pixel digest. Larger images degrade to their
/// dimensions when the encoded header is readable, and to nothing otherwise.
pub const IMAGE_DIGEST_BYTES: usize = 128 * 1024 * 1024;
/// Images with more pixels than this keep only their dimensions.
pub const IMAGE_DIGEST_PIXELS: u64 = 64 * 1024 * 1024;
/// Modification times compared across file systems with coarse timestamps.
pub const MODIFIED_TOLERANCE_MS: i64 = 2000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DigestClass {
    Text,
    Url,
    Html,
    Rtf,
    Image,
    File,
    Other,
}

/// One content class of one logical item.
#[derive(Clone, PartialEq, Eq)]
pub struct Entry {
    pub item: u32,
    pub class: DigestClass,
    /// File: the file name. Other: the native format identity. Otherwise None.
    pub name: Option<String>,
    /// Text: normalized UTF-8 length. Otherwise the native size when known.
    pub size: Option<u64>,
    pub digest: Option<[u8; 32]>,
    pub dimensions: Option<(u32, u32)>,
    pub modified_ms: Option<i64>,
    pub directory: bool,
}
impl Entry {
    pub fn new(item: u32, class: DigestClass) -> Self {
        Self {
            item,
            class,
            name: None,
            size: None,
            digest: None,
            dimensions: None,
            modified_ms: None,
            directory: false,
        }
    }
}
// Names and digests are content-derived; diagnostics show only their shape.
impl fmt::Debug for Entry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Entry")
            .field("item", &self.item)
            .field("class", &self.class)
            .field("size", &self.size)
            .field("digest", &self.digest.is_some())
            .field("dimensions", &self.dimensions)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Fingerprint {
    pub entries: Vec<Entry>,
}
impl Fingerprint {
    /// A single plain-text item (mobile capture and mobile writes).
    pub fn text(text: &str) -> Self {
        Self::rich_text(text, None, None)
    }

    /// A single text item with the rich formats it carries, as a phone
    /// captures or writes it (2026-11-12: phones keep HTML and RTF next to
    /// the plain text). The text entry anchors the comparison like on a PC;
    /// HTML and RTF entries carry their byte size and a digest of their exact
    /// bytes, so the same text with other formatting is a new copy, and the
    /// plain text alone is a format subset of the rich content.
    pub fn rich_text(text: &str, html: Option<&str>, rtf: Option<&str>) -> Self {
        let (digest, size) = text_digest(text);
        let mut entry = Entry::new(0, DigestClass::Text);
        entry.digest = Some(digest);
        entry.size = Some(size);
        let mut entries = vec![entry];
        for (class, value) in [(DigestClass::Html, html), (DigestClass::Rtf, rtf)] {
            let Some(value) = value else { continue };
            let mut entry = Entry::new(0, class);
            entry.digest = Some(rich_digest(class, value.as_bytes()));
            entry.size = Some(value.len() as u64);
            entries.push(entry);
        }
        Self { entries }
    }
}

/// Digest of a rich representation's exact bytes (no normalization: markup
/// is compared as written).
fn rich_digest(class: DigestClass, bytes: &[u8]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(if class == DigestClass::Rtf {
        b"oink/clipboard/dedupe/rtf/1"
    } else {
        b"oink/clipboard/dedupe/htm/1"
    });
    hash.update(bytes);
    *hash.finalize().as_bytes()
}

/// Where the current clipboard content came from (diagnostics only).
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// An Oink device: this device for local copies, `id.clip.origin` for
    /// content received from Oink.
    Device(DeviceId),
    /// Written by another synchronizer and recognised as such.
    External,
}
impl fmt::Debug for Source {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Device(id) => write!(f, "Device({})", id.short()),
            Self::External => f.write_str("External"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Identity {
    pub fingerprint: Fingerprint,
    pub source: Source,
}

/// Why a new clipboard is not an echo of the current one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fresh {
    Empty,
    /// The new content has this class and the old content has none of it.
    Missing(DigestClass),
    /// Both have the class, but the data differs.
    Differs(DigestClass),
    /// The new content's class could not be compared (no digest/metadata).
    Unknown(DigestClass),
    /// Only rich or link formats matched; nothing anchors the content.
    Unanchored,
}

/// Device-local inputs of the content rule (clipboard.md §4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rules {
    /// "Clipboard deduplication" (default on).
    pub dedupe: bool,
    /// This device takes other devices' copies.
    pub receive: bool,
}

/// Why a local change is announced although no marker named it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewCopy {
    /// "Clipboard deduplication" is off: every change is announced.
    DedupeOff,
    /// Receiving is off: this device cannot know the group's current
    /// content, so content is never compared (2026-11-09 D11).
    NotReceiving,
    /// The held content belongs to an older clip than the group's current
    /// one (a newer offer was accepted or seen but not written here):
    /// copying it again is a new copy (2026-11-09 D11).
    EarlierClip,
    /// The content is not a format subset of the held content.
    Fresh(Fresh),
}

/// The content rule on the sending side, after the marker rule: whether a
/// local change is a duplicate of the content this device holds (not
/// announced) or a new copy. Shared by the PC capture and the phone's
/// privileged clipboard capture. `held_is_current`: as far as this device
/// knows, the held content is the group's current clip (no current clip at
/// all, e.g. its source went offline, is not a newer one).
pub fn duplicate(new: &Fingerprint, held: &Identity, rules: Rules, held_is_current: bool) -> Result<(), NewCopy> {
    if !rules.dedupe {
        return Err(NewCopy::DedupeOff);
    }
    if !rules.receive {
        return Err(NewCopy::NotReceiving);
    }
    if !held_is_current {
        return Err(NewCopy::EarlierClip);
    }
    echo(new, held).map_err(NewCopy::Fresh)
}

/// `Ok(())` when `new` is a duplicate of the current clipboard.
pub fn echo(new: &Fingerprint, current: &Identity) -> Result<(), Fresh> {
    subset(new, &current.fingerprint)
}

/// Format subset with the module's tolerance rules.
pub fn subset(new: &Fingerprint, old: &Fingerprint) -> Result<(), Fresh> {
    if new.entries.is_empty() {
        return Err(Fresh::Empty);
    }
    let content: Vec<_> = new.entries.iter().filter(|e| e.class != DigestClass::Other).collect();
    if content.is_empty() {
        for entry in &new.entries {
            let candidates: Vec<_> = old
                .entries
                .iter()
                .filter(|o| o.class == DigestClass::Other && o.name == entry.name)
                .collect();
            if candidates.is_empty() {
                return Err(Fresh::Missing(DigestClass::Other));
            }
            if !candidates
                .iter()
                .any(|o| match (entry.digest, o.digest, entry.size, o.size) {
                    (Some(a), Some(b), ..) => a == b,
                    (_, _, Some(a), Some(b)) => a == b,
                    _ => false,
                })
            {
                return Err(Fresh::Unknown(DigestClass::Other));
            }
        }
        return Ok(());
    }
    let mut anchored = false;
    for entry in content {
        let class = entry.class;
        let candidates: Vec<_> = old.entries.iter().filter(|o| o.class == class).collect();
        if candidates.is_empty() {
            return Err(Fresh::Missing(class));
        }
        match class {
            DigestClass::Text => {
                let Some(digest) = entry.digest else {
                    return Err(Fresh::Unknown(class));
                };
                if !candidates.iter().any(|o| o.digest == Some(digest)) {
                    return Err(if candidates.iter().all(|o| o.digest.is_none()) {
                        Fresh::Unknown(class)
                    } else {
                        Fresh::Differs(class)
                    });
                }
                anchored = true;
            }
            DigestClass::Image => {
                let matched = candidates.iter().any(|o| match (entry.digest, o.digest) {
                    (Some(a), Some(b)) => a == b && entry.dimensions == o.dimensions,
                    // Degraded comparison: one side is above the pixel cost threshold.
                    _ => entry.dimensions.is_some() && entry.dimensions == o.dimensions,
                });
                if !matched {
                    return Err(
                        if entry.dimensions.is_none() || candidates.iter().all(|o| o.dimensions.is_none()) {
                            Fresh::Unknown(class)
                        } else {
                            Fresh::Differs(class)
                        },
                    );
                }
                anchored = true;
            }
            DigestClass::File => {
                let matched = candidates.iter().any(|o| {
                    o.name == entry.name
                        && o.directory == entry.directory
                        && known_equal(entry.size, o.size)
                        && match (entry.modified_ms, o.modified_ms) {
                            (Some(a), Some(b)) => a.abs_diff(b) <= MODIFIED_TOLERANCE_MS as u64,
                            _ => true,
                        }
                });
                if !matched {
                    return Err(Fresh::Differs(class));
                }
                anchored = true;
            }
            DigestClass::Html | DigestClass::Rtf | DigestClass::Url => {
                let matched = candidates.iter().any(|o| match (entry.digest, o.digest) {
                    (Some(a), Some(b)) => a == b,
                    _ => known_equal(entry.size, o.size),
                });
                if !matched {
                    return Err(Fresh::Differs(class));
                }
            }
            DigestClass::Other => unreachable!("filtered above"),
        }
    }
    if anchored { Ok(()) } else { Err(Fresh::Unanchored) }
}

fn known_equal(a: Option<u64>, b: Option<u64>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a == b,
        _ => true,
    }
}

/// Newline-normalized text: CRLF and CR become LF; trailing newlines and NULs
/// (Windows terminators) are removed. Nothing else is changed.
pub fn normalize_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(c);
        }
    }
    let trimmed = out.trim_end_matches(['\n', '\0']).len();
    out.truncate(trimmed);
    out
}

/// Digest and size of the normalized text.
pub fn text_digest(text: &str) -> ([u8; 32], u64) {
    let normalized = normalize_text(text);
    let mut hash = blake3::Hasher::new();
    hash.update(b"oink/clipboard/dedupe/text/1");
    hash.update(normalized.as_bytes());
    (*hash.finalize().as_bytes(), normalized.len() as u64)
}

/// Incremental pixel digest over straight-alpha RGBA8 rows. Fully transparent
/// pixels hash as transparent black: encoders disagree on their color.
#[derive(Debug)]
pub struct PixelDigest(blake3::Hasher);
impl PixelDigest {
    pub fn new(width: u32, height: u32) -> Self {
        let mut hash = blake3::Hasher::new();
        hash.update(b"oink/clipboard/dedupe/image/1");
        hash.update(&width.to_le_bytes());
        hash.update(&height.to_le_bytes());
        Self(hash)
    }
    /// One hash update per row (a per-pixel update costs a call per pixel on
    /// the capture path); the hashed bytes are the same.
    pub fn rgba_row(&mut self, row: &[u8]) {
        let pixels = row.as_chunks::<4>().0;
        if pixels.iter().all(|pixel| pixel[3] != 0) {
            self.0.update(pixels.as_flattened());
            return;
        }
        let cleared: Vec<u8> = pixels
            .iter()
            .flat_map(|pixel| if pixel[3] == 0 { [0; 4] } else { *pixel })
            .collect();
        self.0.update(&cleared);
    }
    pub fn finish(self) -> [u8; 32] {
        *self.0.finalize().as_bytes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn row_digest_hashes_the_same_bytes_as_per_pixel_updates() {
        let rows: [&[u8]; 3] = [&[1, 2, 3, 255, 4, 5, 6, 0, 7], &[9, 9, 9, 9, 8, 8, 8, 8], &[]];
        let mut reference = blake3::Hasher::new();
        reference.update(b"oink/clipboard/dedupe/image/1");
        reference.update(&2u32.to_le_bytes());
        reference.update(&3u32.to_le_bytes());
        let mut digest = PixelDigest::new(2, 3);
        for row in rows {
            for pixel in row.as_chunks::<4>().0 {
                reference.update(if pixel[3] == 0 { &[0; 4] } else { pixel });
            }
            digest.rgba_row(row);
        }
        assert_eq!(digest.finish(), *reference.finalize().as_bytes());
    }

    fn text(item: u32, value: &str) -> Entry {
        let (digest, size) = text_digest(value);
        Entry {
            digest: Some(digest),
            size: Some(size),
            ..Entry::new(item, DigestClass::Text)
        }
    }
    fn image(digest: Option<u8>, w: u32, h: u32) -> Entry {
        Entry {
            digest: digest.map(|d| [d; 32]),
            dimensions: Some((w, h)),
            ..Entry::new(0, DigestClass::Image)
        }
    }
    fn other(name: &str, size: Option<u64>) -> Entry {
        Entry {
            name: Some(name.into()),
            size,
            ..Entry::new(0, DigestClass::Other)
        }
    }
    fn fp(entries: Vec<Entry>) -> Fingerprint {
        Fingerprint { entries }
    }
    fn current(entries: Vec<Entry>, source: Source) -> Identity {
        Identity {
            fingerprint: fp(entries),
            source,
        }
    }
    const A: Source = Source::Device(DeviceId([1; 32]));

    #[test]
    fn format_subset_is_a_duplicate_whatever_the_source() {
        let html = Entry::new(0, DigestClass::Html);
        let current = current(vec![text(0, "hello"), html, other("com.example.private", Some(3))], A);
        // A synchronizer kept only plain text.
        assert_eq!(echo(&fp(vec![text(0, "hello")]), &current), Ok(()));
        // The same device copying the same content again, too.
        assert_eq!(
            echo(&fp(vec![text(0, "hello"), Entry::new(0, DigestClass::Html)]), &current),
            Ok(())
        );
    }

    #[test]
    fn newline_normalization_makes_crlf_cr_and_trailing_newlines_equal() {
        assert_eq!(text_digest("a\r\nb\r\n").0, text_digest("a\nb").0);
        assert_eq!(text_digest("a\rb\n\n").0, text_digest("a\nb").0);
        assert_eq!(text_digest("a\nb\0").0, text_digest("a\nb").0);
        assert_ne!(text_digest("a\n\nb").0, text_digest("a\nb").0);
        assert_ne!(text_digest(" a").0, text_digest("a").0);
        let current = current(vec![text(0, "line1\nline2\n")], A);
        assert_eq!(echo(&fp(vec![text(0, "line1\r\nline2")]), &current), Ok(()));
    }

    #[test]
    fn a_text_substring_is_a_new_copy() {
        let current = current(vec![text(0, "hello world")], A);
        assert_eq!(
            echo(&fp(vec![text(0, "hello")]), &current),
            Err(Fresh::Differs(DigestClass::Text))
        );
    }

    #[test]
    fn rich_text_only_in_the_new_content_is_not_a_subset() {
        let plain = current(vec![text(0, "hello")], A);
        assert_eq!(
            echo(&fp(vec![text(0, "hello"), Entry::new(0, DigestClass::Html)]), &plain),
            Err(Fresh::Missing(DigestClass::Html))
        );
        // Rich formats alone never anchor an echo.
        let rich = current(vec![Entry::new(0, DigestClass::Rtf)], A);
        assert_eq!(
            echo(&fp(vec![Entry::new(0, DigestClass::Rtf)]), &rich),
            Err(Fresh::Unanchored)
        );
    }

    #[test]
    fn synchronizer_tag_formats_do_not_defeat_a_text_match() {
        let plain = current(vec![text(0, "hello")], A);
        assert_eq!(
            echo(&fp(vec![text(0, "hello"), other("com.vendor.sync-tag", None)]), &plain),
            Ok(())
        );
        // Opaque-only content compares exactly.
        let opaque = current(vec![other("com.example.private", Some(7))], A);
        assert_eq!(echo(&fp(vec![other("com.example.private", Some(7))]), &opaque), Ok(()));
        assert_eq!(
            echo(&fp(vec![other("com.example.private", Some(8))]), &opaque),
            Err(Fresh::Unknown(DigestClass::Other))
        );
        assert_eq!(
            echo(&fp(vec![other("com.example.private", None)]), &opaque),
            Err(Fresh::Unknown(DigestClass::Other))
        );
    }

    #[test]
    fn reencoded_images_compare_by_pixels_and_degrade_to_dimensions() {
        let current = current(vec![image(Some(9), 10, 20)], A);
        assert_eq!(echo(&fp(vec![image(Some(9), 10, 20)]), &current), Ok(()));
        assert_eq!(
            echo(&fp(vec![image(Some(8), 10, 20)]), &current),
            Err(Fresh::Differs(DigestClass::Image))
        );
        // Above the pixel threshold on one side: dimensions decide.
        assert_eq!(echo(&fp(vec![image(None, 10, 20)]), &current), Ok(()));
        assert_eq!(
            echo(&fp(vec![image(None, 11, 20)]), &current),
            Err(Fresh::Differs(DigestClass::Image))
        );
        let unknown = Entry::new(0, DigestClass::Image);
        assert_eq!(
            echo(&fp(vec![unknown]), &current),
            Err(Fresh::Unknown(DigestClass::Image))
        );
    }

    #[test]
    fn files_compare_by_metadata() {
        let file = |name: &str, size: u64, modified: i64| Entry {
            name: Some(name.into()),
            size: Some(size),
            modified_ms: Some(modified),
            ..Entry::new(0, DigestClass::File)
        };
        let current = current(vec![file("a.txt", 5, 10_000), file("b.txt", 6, 10_000)], A);
        assert_eq!(echo(&fp(vec![file("a.txt", 5, 11_000)]), &current), Ok(()));
        for changed in [
            file("a.txt", 4, 10_000),
            file("a.txt", 5, 20_000),
            file("c.txt", 5, 10_000),
        ] {
            assert_eq!(
                echo(&fp(vec![changed]), &current),
                Err(Fresh::Differs(DigestClass::File))
            );
        }
    }

    #[test]
    fn plain_text_is_a_subset_of_rich_text_but_not_the_reverse() {
        let rich = Identity {
            fingerprint: Fingerprint::rich_text("hello", Some("<b>hello</b>"), Some("{\\rtf1 hello}")),
            source: A,
        };
        // A synchronizer kept only the plain text, or only HTML and text.
        assert_eq!(echo(&Fingerprint::text("hello\r\n"), &rich), Ok(()));
        assert_eq!(
            echo(&Fingerprint::rich_text("hello", Some("<b>hello</b>"), None), &rich),
            Ok(())
        );
        // The same text with other formatting is a new copy.
        assert_eq!(
            echo(&Fingerprint::rich_text("hello", Some("<i>hello</i>"), None), &rich),
            Err(Fresh::Differs(DigestClass::Html))
        );
        // Rich content after the plain text is a new copy.
        let plain = Identity {
            fingerprint: Fingerprint::text("hello"),
            source: A,
        };
        assert_eq!(
            echo(&Fingerprint::rich_text("hello", Some("<b>hello</b>"), None), &plain),
            Err(Fresh::Missing(DigestClass::Html))
        );
    }

    #[test]
    fn the_content_rule_yields_to_the_setting_receiving_and_a_newer_group_clip() {
        let held = Identity {
            fingerprint: Fingerprint::text("same"),
            source: A,
        };
        let new = Fingerprint::text("same");
        let on = Rules {
            dedupe: true,
            receive: true,
        };
        assert_eq!(duplicate(&new, &held, on, true), Ok(()));
        assert_eq!(
            duplicate(&new, &held, Rules { dedupe: false, ..on }, true),
            Err(NewCopy::DedupeOff)
        );
        assert_eq!(
            duplicate(&new, &held, Rules { receive: false, ..on }, true),
            Err(NewCopy::NotReceiving)
        );
        assert_eq!(duplicate(&new, &held, on, false), Err(NewCopy::EarlierClip));
        assert_eq!(
            duplicate(&Fingerprint::text("other"), &held, on, true),
            Err(NewCopy::Fresh(Fresh::Differs(DigestClass::Text)))
        );
    }

    #[test]
    fn empty_content_is_never_an_echo() {
        let current = current(vec![text(0, "x")], A);
        assert_eq!(echo(&fp(vec![]), &current), Err(Fresh::Empty));
    }

    #[test]
    fn pixel_digest_ignores_the_color_of_transparent_pixels() {
        let mut a = PixelDigest::new(2, 1);
        a.rgba_row(&[1, 2, 3, 0, 9, 9, 9, 255]);
        let mut b = PixelDigest::new(2, 1);
        b.rgba_row(&[0, 0, 0, 0, 9, 9, 9, 255]);
        assert_eq!(a.finish(), b.finish());
        let mut c = PixelDigest::new(1, 2);
        c.rgba_row(&[0, 0, 0, 0, 9, 9, 9, 255]);
        assert_ne!(c.finish(), PixelDigest::new(2, 1).finish());
    }
}
