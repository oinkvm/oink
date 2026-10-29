//! Per-read clipboard identity; no IDataObject or payload survives a request.
use super::*;
use std::sync::{Mutex, Weak};
use windows::Win32::System::DataExchange::{GetClipboardOwner, GetClipboardSequenceNumber};

#[derive(Default, Debug)]
pub(crate) struct Observer(Mutex<Weak<Identity>>);
impl Observer {
    pub fn logical(&self, count: u64) -> u64 {
        let Some(identity) = self.0.lock().unwrap().upgrade() else {
            return count;
        };
        let state = identity.state.lock().unwrap();
        // Only the same owner can be completing our delayed render. A real
        // ownership change must reach the watcher even while the old COM read
        // is blocked; otherwise an unresponsive source hides every later copy.
        // This query reads window-station metadata and never invokes GetData.
        // SAFETY: owner lookup takes no pointers and is valid off the owner STA.
        let owner = unsafe { GetClipboardOwner() }.ok().map(|hwnd| hwnd.0 as isize);
        if state.sequence == count || (state.reading && owner.is_some() && state.read_owner == owner) {
            identity.announced
        } else {
            count
        }
    }
    pub fn set(&self, identity: &Arc<Identity>) {
        *self.0.lock().unwrap() = Arc::downgrade(identity);
    }
}
#[derive(Debug)]
struct State {
    sequence: u64,
    reading: bool,
    read_owner: Option<isize>,
}
#[derive(Debug)]
pub(crate) struct Identity {
    announced: u64,
    state: Mutex<State>,
}
pub(super) fn sequence() -> u64 {
    // SAFETY: query for this process's window station, no handles/pointers.
    u64::from(unsafe { GetClipboardSequenceNumber() })
}
impl Identity {
    pub fn new(count: u64) -> Arc<Self> {
        Arc::new(Self {
            announced: count,
            state: Mutex::new(State {
                sequence: count,
                reading: false,
                read_owner: None,
            }),
        })
    }
    pub fn clipboard_key(&self) -> io::Result<[u8; 32]> {
        self.check()?;
        if self.state.lock().unwrap().reading {
            return Err(io::ErrorKind::WouldBlock.into());
        }
        let key = super::super::clipboard_key::current()?;
        self.check()?;
        Ok(key)
    }
    pub fn check(&self) -> io::Result<()> {
        if self.state.lock().unwrap().sequence == sequence() {
            Ok(())
        } else {
            Err(content::clipboard::changed())
        }
    }
    /// Caller serializes reads on its dedicated STA. The observer never waits on
    /// native rendering; the small state mutex is released before any COM call.
    pub fn read<T>(&self, offer: &Inventory, read: impl FnOnce(&IDataObject) -> io::Result<T>) -> io::Result<T> {
        self.check()?;
        // SAFETY: called on the reader STA; no COM pointers leave this closure.
        let owner = unsafe { GetClipboardOwner() }.ok();
        let before = sequence();
        {
            let mut state = self.state.lock().unwrap();
            if state.sequence != before {
                return Err(content::clipboard::changed());
            }
            state.reading = true;
            state.read_owner = owner.map(|hwnd| hwnd.0 as isize);
        }
        let result = (|| {
            // SAFETY: short-lived clipboard object on initialized STA.
            let data = unsafe { OleGetClipboard() }.map_err(|_| content::unavailable(Unavailable::ReadFailed))?;
            let value = read(&data);
            drop(data);
            let after = sequence();
            if after != before {
                // Delayed rendering itself changes GetClipboardSequenceNumber.
                // OleIsCurrentClipboard cannot validate an OleGetClipboard consumer.
                // Accept our own bump only if owner + freshly enumerated formats
                // match. A same-app same-format copy during this read is inherently
                // indistinguishable, accepted as local-paste-at-read-time semantics.
                // Do not render everything at copy time or retain a fallback copy.
                // https://learn.microsoft.com/windows/win32/api/winuser/nf-winuser-getclipboardsequencenumber
                // https://learn.microsoft.com/windows/win32/api/ole2/nf-ole2-oleiscurrentclipboard
                // SAFETY: same initialized STA; the new object is released here.
                let current = unsafe { OleGetClipboard() }.map_err(|_| content::clipboard::changed())?;
                // SAFETY: owner query has no input pointers and does not change ownership.
                let after_owner = unsafe { GetClipboardOwner() }.ok();
                if owner.is_none()
                    || after_owner != owner
                    || !same_formats(&inventory(&current, offer.id)?, offer)
                    || sequence() != after
                {
                    return Err(content::clipboard::changed());
                }
                self.state.lock().unwrap().sequence = after;
            }
            value
        })();
        self.state.lock().unwrap().reading = false;
        result
    }
}

fn same_formats(left: &Inventory, right: &Inventory) -> bool {
    left.items.len() == right.items.len()
        && left.items.iter().zip(&right.items).all(|(left, right)| {
            left.id == right.id
                && left.representations.len() == right.representations.len()
                && left
                    .representations
                    .iter()
                    .zip(&right.representations)
                    .all(|(left, right)| {
                        (left.id, &left.format, left.medium, left.aspect, left.index)
                            == (right.id, &right.format, right.medium, right.aspect, right.index)
                    })
        })
}
