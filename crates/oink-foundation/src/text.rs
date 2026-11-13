//! Committed text split for native Unicode insertion (`InjectEvent::Text`).
//!
//! Both desktop platforms insert UTF-16 code units, not key codes. macOS
//! limits one keyboard event's Unicode payload and Windows paces long text
//! (see `oink_input`); both split with [`utf16_chunks`], and the router uses
//! it to slice paced text (`TextPacing`).

/// Split `text` into UTF-16 chunks of at most `max` (>= 2) units.
///
/// A chunk never ends inside a surrogate pair. It also prefers to end only
/// between user-perceived characters, so that an emoji ZWJ sequence, a flag,
/// a keycap or a base letter with its combining marks reaches the target in
/// one insertion. Only a single such cluster longer than `max` is split, and
/// then only between scalar values. Concatenating the chunks always gives
/// back exactly `text`. The cluster rule is a deliberate approximation of
/// UAX #29 (no Unicode tables): a wrong guess only costs one more insertion.
pub fn utf16_chunks(text: &str, max: usize) -> Vec<Vec<u16>> {
    assert!(max >= 2, "a chunk must hold a surrogate pair");
    let mut chunks = Vec::new();
    let mut chunk: Vec<u16> = Vec::new();
    for cluster in clusters(text) {
        let units: Vec<u16> = cluster.encode_utf16().collect();
        if chunk.len() + units.len() <= max {
            chunk.extend_from_slice(&units);
            continue;
        }
        if !chunk.is_empty() {
            chunks.push(std::mem::take(&mut chunk));
        }
        if units.len() <= max {
            chunk = units;
            continue;
        }
        for c in cluster.chars() {
            let mut buffer = [0; 2];
            let scalar = c.encode_utf16(&mut buffer);
            if chunk.len() + scalar.len() > max {
                chunks.push(std::mem::take(&mut chunk));
            }
            chunk.extend_from_slice(scalar);
        }
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    chunks
}

/// Approximate extended grapheme clusters of `text`.
fn clusters(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut previous: Option<char> = None;
    let mut regional_run = 0usize;
    for (index, c) in text.char_indices() {
        if let Some(p) = previous {
            let joined = extends(c)
                || p == ZWJ
                || (is_regional(p) && is_regional(c) && regional_run % 2 == 1)
                || (is_hangul_leading(p) && is_hangul_vowel_or_trailing(c));
            if !joined {
                out.push(&text[start..index]);
                start = index;
            }
        }
        regional_run = if is_regional(c) { regional_run + 1 } else { 0 };
        previous = Some(c);
    }
    if start < text.len() {
        out.push(&text[start..]);
    }
    out
}

const ZWJ: char = '\u{200D}';

/// Characters that attach to the preceding one.
fn extends(c: char) -> bool {
    matches!(c as u32,
        0x200D                // zero width joiner
        | 0x200C              // zero width non-joiner
        | 0x0300..=0x036F     // combining diacritical marks
        | 0x0483..=0x0489
        | 0x0591..=0x05C7     // Hebrew points
        | 0x0610..=0x061A | 0x064B..=0x065F | 0x0670 | 0x06D6..=0x06ED // Arabic marks
        | 0x0900..=0x0903 | 0x093A..=0x094F | 0x0951..=0x0957 | 0x0962..=0x0963 // Devanagari signs
        | 0x0E31 | 0x0E34..=0x0E3A | 0x0E47..=0x0E4E // Thai
        | 0x1AB0..=0x1AFF | 0x1DC0..=0x1DFF
        | 0x20D0..=0x20FF     // combining marks for symbols, incl. keycap U+20E3
        | 0x3099..=0x309A     // kana voicing marks
        | 0xFE00..=0xFE0F     // variation selectors
        | 0xFE20..=0xFE2F
        | 0x1F3FB..=0x1F3FF   // emoji skin tone modifiers
        | 0xE0020..=0xE007F   // emoji tag sequences
        | 0xE0100..=0xE01EF   // variation selectors supplement
    )
}

fn is_regional(c: char) -> bool {
    ('\u{1F1E6}'..='\u{1F1FF}').contains(&c)
}

fn is_hangul_leading(c: char) -> bool {
    ('\u{1100}'..='\u{115F}').contains(&c) || ('\u{1160}'..='\u{11A7}').contains(&c)
}

fn is_hangul_vowel_or_trailing(c: char) -> bool {
    ('\u{1160}'..='\u{11FF}').contains(&c)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The macOS per-event limit (`oink_input::text::MAC_EVENT_UNITS`).
    const MAC_EVENT_UNITS: usize = 20;
    const FAMILY: &str = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";

    fn joined(chunks: &[Vec<u16>]) -> String {
        String::from_utf16(&chunks.concat()).unwrap()
    }
    fn no_split_pair(chunks: &[Vec<u16>]) -> bool {
        chunks.iter().all(|chunk| {
            !chunk.last().is_some_and(|unit| (0xD800..0xDC00).contains(unit))
                && !chunk.first().is_some_and(|unit| (0xDC00..0xE000).contains(unit))
        })
    }

    #[test]
    fn chunks_reassemble_and_respect_the_unit_limit() {
        let text = format!(
            "中文 English 😀 \u{1F468}\u{200D}\u{1F469} {FAMILY} {}",
            "長".repeat(57)
        );
        for max in [2, 3, 4, 7, MAC_EVENT_UNITS, 64] {
            let chunks = utf16_chunks(&text, max);
            assert_eq!(joined(&chunks), text, "max {max}");
            assert!(chunks.iter().all(|chunk| !chunk.is_empty() && chunk.len() <= max));
            assert!(no_split_pair(&chunks), "max {max}");
        }
    }

    #[test]
    fn surrogate_pairs_are_never_split_even_at_odd_limits() {
        let text = "a😀😀😀b🎉";
        for max in [2, 3, 5] {
            let chunks = utf16_chunks(text, max);
            assert!(no_split_pair(&chunks), "{chunks:?}");
            assert_eq!(joined(&chunks), text);
        }
        assert_eq!(
            utf16_chunks("😀😀", 3),
            vec![vec![0xD83D, 0xDE00], vec![0xD83D, 0xDE00]]
        );
    }

    #[test]
    fn zwj_flags_keycaps_and_marks_stay_in_one_insertion() {
        // 19 units of filler leave one free unit: the next cluster moves whole.
        let filler = "x".repeat(19);
        for cluster in [
            "\u{1F468}\u{200D}\u{1F469}",                                      // ZWJ pair, 5 units
            FAMILY,                                                            // 11 units
            "\u{1F1E8}\u{1F1F3}",                                              // flag (regional indicator pair)
            "1\u{FE0F}\u{20E3}",                                               // keycap
            "e\u{0301}\u{0302}",                                               // combining marks
            "\u{1F44D}\u{1F3FD}",                                              // skin tone
            "\u{1F3F4}\u{E0067}\u{E0062}\u{E0065}\u{E006E}\u{E0067}\u{E007F}", // tag sequence
        ] {
            let text = format!("{filler}{cluster}");
            let chunks = utf16_chunks(&text, MAC_EVENT_UNITS);
            assert_eq!(chunks.len(), 2, "{cluster:?}");
            assert_eq!(String::from_utf16(&chunks[1]).unwrap(), cluster);
        }
        // Two flags are two clusters, not one four-indicator cluster.
        let flags = "\u{1F1E8}\u{1F1F3}\u{1F1EF}\u{1F1F5}";
        assert_eq!(utf16_chunks(flags, 4).len(), 2);
    }

    #[test]
    fn a_cluster_longer_than_the_limit_splits_only_between_scalars() {
        let long = format!("{FAMILY}\u{200D}{FAMILY}"); // 23 units, one cluster
        let chunks = utf16_chunks(&long, MAC_EVENT_UNITS);
        assert_eq!(chunks.len(), 2);
        assert!(no_split_pair(&chunks));
        assert_eq!(joined(&chunks), long);
    }
}
