//! One bounded, numeric-only summary of a failed native target drag.
//! Observes existing COM calls; never requests formats, bodies or Shell metadata.
use super::*;
use std::sync::Mutex;

const LIMIT: usize = 64;
type Row = [i64; 10];

pub(in crate::windows) struct Session {
    operation: DragOperation,
    began: Instant,
    offered: u32,
    ids: [u32; 12],
    state: Mutex<State>,
}
#[derive(Clone, Default)]
struct State {
    rows: Vec<Row>,
    truncated: bool,
    feedback: u32,
    feedback_calls: u32,
    feedback_seen: u32,
    enters: u32,
    leaves: u32,
    continuation: i32,
    keys: u32,
    cancelled: bool,
    escaped: bool,
    async_started: u32,
    async_ended: u32,
    async_result: Option<i32>,
    async_effect: u32,
    receipts: [Option<u32>; 3],
    // CIDA generations, last item count, structural validity, direct children,
    // parent SHITEMID count. This says nothing about SFGAO/permission acceptance.
    cida: [u64; 5],
}
impl Session {
    pub(in crate::windows) fn new(operation: DragOperation, offered: DROPEFFECT) -> Arc<Self> {
        // SAFETY: fixed, NUL-terminated format names; no clipboard contents accessed.
        let ids = unsafe {
            [
                15,
                RegisterClipboardFormatW(w!("Shell IDList Array")),
                RegisterClipboardFormatW(w!("FileGroupDescriptorW")),
                RegisterClipboardFormatW(w!("FileGroupDescriptor")),
                RegisterClipboardFormatW(w!("FileContents")),
                RegisterClipboardFormatW(w!("Preferred DropEffect")),
                RegisterClipboardFormatW(w!("Performed DropEffect")),
                RegisterClipboardFormatW(w!("Logical Performed DropEffect")),
                RegisterClipboardFormatW(w!("Paste Succeeded")),
                RegisterClipboardFormatW(w!("FileNameW")),
                RegisterClipboardFormatW(w!("FileName")),
                RegisterClipboardFormatW(w!("TargetCLSID")),
            ]
        };
        Arc::new(Self {
            operation,
            began: Instant::now(),
            offered: offered.0,
            ids,
            state: Mutex::new(State::default()),
        })
    }
    /// stage: 0 offered, 1 QueryGetData result, 2 GetData result.
    pub(in crate::windows) fn format(&self, stage: usize, format: &FORMATETC, result: i32) {
        let mut state = self.state.lock().unwrap();
        let kind = self
            .ids
            .iter()
            .position(|id| *id == u32::from(format.cfFormat))
            .map_or(0, |i| i + 1);
        let key = [
            i64::from(format.cfFormat),
            kind as i64,
            i64::from(format.lindex),
            i64::from(format.dwAspect),
            i64::from(format.tymed),
        ];
        let at = state.rows.iter().position(|row| row[..5] == key);
        let at = match at {
            Some(at) => at,
            None if state.rows.len() < LIMIT => {
                let mut row = [0; 10];
                row[..5].copy_from_slice(&key);
                state.rows.push(row);
                state.rows.len() - 1
            }
            None => {
                state.truncated = true;
                return;
            }
        };
        let row = &mut state.rows[at];
        match stage {
            0 => row[5] = 1,
            1 => {
                row[6] += 1;
                row[7] = i64::from(result);
            }
            2 => {
                row[8] += 1;
                row[9] = i64::from(result);
            }
            _ => unreachable!(),
        }
    }
    pub(in crate::windows) fn bytes(&self, format: u32, bytes: &[u8]) {
        if format == self.ids[1] {
            let mut state = self.state.lock().unwrap();
            let count = state.cida[0] + 1;
            state.cida = cida_shape(bytes);
            state.cida[0] = count;
        }
    }
    pub(in crate::windows) fn receipt(&self, format: u16, bytes: &[u8]) {
        if let Some(at) = self.ids[6..9].iter().position(|id| *id == u32::from(format))
            && let Some(value) = bytes.get(..4)
        {
            self.state.lock().unwrap().receipts[at] = Some(u32::from_le_bytes(value.try_into().unwrap()));
        }
    }
    pub(in crate::windows) fn async_start(&self) {
        self.state.lock().unwrap().async_started += 1;
    }
    pub(in crate::windows) fn async_end(&self, result: i32, effect: u32) {
        let mut state = self.state.lock().unwrap();
        state.async_ended += 1;
        state.async_result = Some(result);
        state.async_effect = effect;
    }
    pub(super) fn feedback(&self, effect: DROPEFFECT) {
        let mut state = self.state.lock().unwrap();
        state.feedback = effect.0;
        state.feedback_calls += 1;
        state.feedback_seen |= effect.0;
    }
    pub(super) fn target(&self, entered: bool) {
        let mut state = self.state.lock().unwrap();
        if entered {
            state.enters += 1;
        } else {
            state.leaves += 1;
        }
    }
    pub(super) fn continuation(&self, result: HRESULT, keys: MODIFIERKEYS_FLAGS, cancelled: bool, escaped: bool) {
        let mut state = self.state.lock().unwrap();
        state.continuation = result.0;
        state.keys = keys.0;
        state.cancelled |= cancelled;
        state.escaped |= escaped;
    }
    fn failed(&self, dropped: bool) -> Option<State> {
        (!dropped).then(|| self.state.lock().unwrap().clone())
    }
    pub(super) fn finish(&self, hr: HRESULT, effect: DROPEFFECT, dropped: bool) {
        let Some(state) = self.failed(dropped) else {
            return;
        };
        let formats = serde_json::to_string(&state.rows).expect("numeric rows");
        let cida = serde_json::to_string(&state.cida).expect("numeric CIDA shape");
        tracing::info!(
            dnd_hold_sequence = self.operation.sequence,
            dnd_win_drop_schema = 1_u64,
            dnd_win_drop_offered = self.offered,
            dnd_win_drop_hresult = hr.0,
            dnd_win_drop_effect = effect.0,
            dnd_win_drop_dropped = dropped,
            dnd_win_drop_feedback = state.feedback,
            dnd_win_drop_feedback_calls = state.feedback_calls,
            dnd_win_drop_feedback_seen = state.feedback_seen,
            dnd_win_drop_enters = state.enters,
            dnd_win_drop_leaves = state.leaves,
            dnd_win_drop_continue = state.continuation,
            dnd_win_drop_keys = state.keys,
            dnd_win_drop_cancel = state.cancelled,
            dnd_win_drop_escape = state.escaped,
            dnd_win_drop_elapsed_us = self.began.elapsed().as_micros() as u64,
            dnd_win_drop_async_started = state.async_started,
            dnd_win_drop_async_ended = state.async_ended,
            dnd_win_drop_async_result = state.async_result.unwrap_or(i32::MIN),
            dnd_win_drop_async_effect = state.async_effect,
            dnd_win_drop_performed = state.receipts[0].map_or(-1, i64::from),
            dnd_win_drop_logical = state.receipts[1].map_or(-1, i64::from),
            dnd_win_drop_paste = state.receipts[2].map_or(-1, i64::from),
            dnd_win_drop_truncated = state.truncated,
            dnd_win_drop_formats = formats.as_str(),
            dnd_win_drop_cida = cida.as_str(),
            "native Windows target drag ended without drop"
        );
    }
}

fn cida_shape(bytes: &[u8]) -> [u64; 5] {
    let mut result = [0; 5];
    let Some(count) = bytes
        .get(..4)
        .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
    else {
        return result;
    };
    result[1] = count as u64;
    if count > 65_535 {
        return result;
    }
    let header = 4 + (count + 1) * 4;
    if header > bytes.len() {
        return result;
    }
    for index in 0..=count {
        let at = 4 + index * 4;
        let mut cursor = u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        if cursor < header {
            return result;
        }
        let mut components = 0;
        loop {
            let Some(size) = bytes
                .get(cursor..cursor.saturating_add(2))
                .map(|b| u16::from_le_bytes(b.try_into().unwrap()) as usize)
            else {
                return result;
            };
            if size == 0 {
                break;
            }
            if size < 2 || cursor.saturating_add(size) > bytes.len() {
                return result;
            }
            cursor += size;
            components += 1;
        }
        if index == 0 {
            result[4] = components;
        } else if components == 1 {
            result[3] += 1;
        }
    }
    result[2] = 1;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_summary_preserves_none_effect_receipts_and_bounds_unknown_formats() {
        let trace = Session::new(DragOperation::new(), DROPEFFECT_COPY | DROPEFFECT_MOVE);
        for index in 0..70 {
            let format = FORMATETC {
                cfFormat: 15,
                lindex: index,
                dwAspect: DVASPECT_CONTENT.0,
                tymed: TYMED_HGLOBAL.0 as u32,
                ..Default::default()
            };
            trace.format(0, &format, 0);
            trace.format(1, &format, S_OK.0);
            trace.format(2, &format, windows::Win32::Foundation::DV_E_FORMATETC.0);
        }
        trace.receipt(trace.ids[7] as u16, &2u32.to_le_bytes());
        trace.receipt(trace.ids[6] as u16, &0u32.to_le_bytes());
        trace.async_start();
        trace.feedback(DROPEFFECT(0x8000_0002));
        trace.continuation(DRAGDROP_S_DROP, MODIFIERKEYS_FLAGS(0), false, false);
        let state = trace.failed(false).unwrap();
        assert!(trace.failed(true).is_none());
        assert!(state.truncated);
        assert_eq!(state.rows.len(), LIMIT);
        assert_eq!(state.rows[0], [15, 1, 0, 1, 1, 1, 1, 0, 1, -2147221404]);
        assert_eq!(state.receipts, [Some(0), Some(2), None]);
        assert_eq!(state.async_started, 1);
        assert_eq!(state.async_ended, 0);
        assert_eq!(state.feedback, 0x8000_0002);
        assert!(!state.cancelled);
        let encoded = serde_json::to_string(&state.rows).unwrap();
        assert_eq!(serde_json::from_str::<Vec<Row>>(&encoded).unwrap(), state.rows);
    }
    #[test]
    fn cida_shape_distinguishes_multilevel_children_and_bad_offsets() {
        let bytes = [1, 0, 0, 0, 12, 0, 0, 0, 16, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0];
        assert_eq!(cida_shape(&bytes), [0, 1, 1, 1, 1]);
        let mut nested = bytes.to_vec();
        nested.extend([0, 0]);
        nested[18] = 2;
        assert_eq!(cida_shape(&nested), [0, 1, 1, 0, 1]);
        let mut invalid = bytes;
        invalid[8] = 255;
        assert_eq!(cida_shape(&invalid)[2], 0);
    }
}
