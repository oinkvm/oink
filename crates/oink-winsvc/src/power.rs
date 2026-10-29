//! Sanitized class-45 snapshot. The private NT buffer never crosses the pipe.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub display: bool,
    pub system: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    V3,
    V4,
}
impl Layout {
    pub fn windows(major: u32, build: u32) -> Option<Self> {
        match (major, build) {
            (10, 10240 | 10586) => Some(Self::V3),
            (10, 14393..=26200) => Some(Self::V4),
            _ => None,
        }
    }
}
/// x64 only. Checks every native offset before reading; never dereferences native pointers.
/// A failed PID lookup excludes that row rather than risking an Oink feedback loop.
pub fn parse(bytes: &[u8], layout: Layout, mut excluded: impl FnMut(u32) -> bool) -> Result<Summary, &'static str> {
    fn u32at(b: &[u8], at: usize) -> Result<u32, &'static str> {
        Ok(u32::from_le_bytes(
            b.get(at..at.checked_add(4).ok_or("overflow")?)
                .ok_or("short u32")?
                .try_into()
                .unwrap(),
        ))
    }
    fn usizeat(b: &[u8], at: usize) -> Result<usize, &'static str> {
        usize::try_from(u64::from_le_bytes(
            b.get(at..at.checked_add(8).ok_or("overflow")?)
                .ok_or("short offset")?
                .try_into()
                .unwrap(),
        ))
        .map_err(|_| "large offset")
    }
    let count = usizeat(bytes, 0)?;
    if count > 4096 {
        return Err("request count limit");
    }
    let header = (count + 1) * 8;
    if header > bytes.len() {
        return Err("short offset table");
    }
    let mut summary = Summary::default();
    let mut ranges = Vec::with_capacity(count);
    let diag = match layout {
        Layout::V3 => 24,
        Layout::V4 => 32,
    };
    for index in 0..count {
        let at = usizeat(bytes, 8 + index * 8)?;
        if at < header || at % 8 != 0 {
            return Err("invalid request offset");
        }
        let record = bytes.get(at..).ok_or("request outside buffer")?;
        let mask = u32at(record, 0)?;
        let allowed = match layout {
            Layout::V3 => 0x1f,
            Layout::V4 => 0x3f,
        };
        if mask & !allowed != 0 {
            return Err("unknown request mask");
        }
        let size = usizeat(record, diag)?;
        if size < 40 || size > record.len().saturating_sub(diag) {
            return Err("invalid diagnostic size");
        }
        let end = at
            .checked_add(diag)
            .and_then(|n| n.checked_add(size))
            .ok_or("request range overflow")?;
        if ranges.iter().any(|&(start, stop)| at < stop && start < end) {
            return Err("overlapping requests");
        }
        ranges.push((at, end));
        let diagnostic = &record[diag..diag + size];
        let caller = u32at(diagnostic, 8)?;
        if caller > 2 {
            return Err("unknown caller kind");
        }
        // Native string/reason offsets are relative to DIAGNOSTIC_BUFFER. We do
        // not decode sensitive strings, but still reject impossible offsets.
        for offset_at in [16, 32] {
            let offset = usizeat(diagnostic, offset_at)?;
            if offset != 0 && (offset < 40 || offset >= size || offset % 2 != 0) {
                return Err("invalid diagnostic offset");
            }
        }
        if caller == 0 {
            let path_offset = usizeat(diagnostic, 24)?;
            if path_offset != 0 && (path_offset < 40 || path_offset >= size || path_offset % 2 != 0) {
                return Err("invalid driver path offset");
            }
            continue;
        } // Kernel/driver: no process attribution.
        let pid = u32at(diagnostic, 24)?;
        if pid == 0 || excluded(pid) {
            continue;
        }
        let present = |kind: usize| -> Result<bool, &'static str> {
            Ok(mask & (1 << kind) != 0 && u32at(record, 4 + kind * 4)? != 0)
        };
        summary.display |= present(0)?;
        summary.system |= present(1)? || present(3)?;
    }
    Ok(summary)
}
#[cfg(test)]
mod tests {
    use super::*;
    fn list(layout: Layout, pid: u32, caller: u32, counts: [u32; 4]) -> Vec<u8> {
        let diag = if layout == Layout::V3 { 24 } else { 32 };
        let mut b = vec![0; 16 + diag + 40];
        b[..8].copy_from_slice(&1u64.to_le_bytes());
        b[8..16].copy_from_slice(&16u64.to_le_bytes());
        b[16..20].copy_from_slice(&15u32.to_le_bytes());
        for (i, c) in counts.into_iter().enumerate() {
            b[20 + i * 4..24 + i * 4].copy_from_slice(&c.to_le_bytes());
        }
        let d = 16 + diag;
        b[d..d + 8].copy_from_slice(&40u64.to_le_bytes());
        b[d + 8..d + 12].copy_from_slice(&caller.to_le_bytes());
        b[d + 24..d + 28].copy_from_slice(&(if caller == 0 { 0 } else { pid }).to_le_bytes());
        b
    }
    #[test]
    fn mapping_and_self_filter_for_both_layouts() {
        for v in [Layout::V3, Layout::V4] {
            assert_eq!(
                parse(&list(v, 42, 1, [1, 0, 0, 0]), v, |_| false),
                Ok(Summary {
                    display: true,
                    system: false
                })
            );
            assert_eq!(
                parse(&list(v, 42, 1, [0, 0, 0, 1]), v, |_| false),
                Ok(Summary {
                    display: false,
                    system: true
                })
            );
            assert_eq!(
                parse(&list(v, 42, 1, [1, 1, 0, 1]), v, |pid| pid == 42),
                Ok(Summary::default())
            );
            assert_eq!(
                parse(&list(v, 42, 0, [1, 1, 0, 1]), v, |_| false),
                Ok(Summary::default())
            );
            assert_eq!(
                parse(&list(v, 42, 1, [0, 0, 1, 0]), v, |_| false),
                Ok(Summary::default())
            );
            assert!(parse(&list(v, 42, 3, [0; 4]), v, |_| false).is_err());
        }
    }
    #[test]
    fn malformed_native_buffers_are_unavailable() {
        let good = list(Layout::V4, 42, 1, [1, 1, 1, 1]);
        for end in 0..good.len() {
            assert!(parse(&good[..end], Layout::V4, |_| false).is_err());
        }
        for at in [0, 8, 16, 48, 64, 80] {
            let mut b = good.clone();
            b[at..at + 4].fill(255);
            assert!(parse(&b, Layout::V4, |_| false).is_err(), "offset {at}");
        }
        assert_eq!(Layout::windows(10, 10586), Some(Layout::V3));
        assert_eq!(Layout::windows(10, 26100), Some(Layout::V4));
        assert_eq!(Layout::windows(11, 30000), None);
    }
}
