//! Windows file-format policy and the delivery retention rule, kept independent
//! of the OS so fake native publishers can exercise the same decisions.
use oink_foundation::clipboard::ClipPlatform;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Formats {
    pub hdrop: bool,
    pub virtual_files: bool,
}
pub(crate) fn formats(_platform: ClipPlatform, virtual_source: bool, available: bool) -> Formats {
    Formats {
        hdrop: available,
        // Real paths never gain stream/promise formats. One-shot sources keep
        // their original stream alongside the received path; both require it.
        virtual_files: available && virtual_source,
    }
}

pub(crate) const RETENTION_SECS: u64 = 24 * 60 * 60;
pub(crate) fn retire(age: u64, consumed: bool, native_owners: usize, readers: usize) -> bool {
    readers == 0 && (age >= RETENTION_SECS || (consumed && native_owners == 0))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_format_selection() {
        assert_eq!(
            formats(ClipPlatform::Mac, false, true),
            Formats {
                hdrop: true,
                virtual_files: false
            }
        );
        assert_eq!(
            formats(ClipPlatform::Windows, true, true),
            Formats {
                hdrop: true,
                virtual_files: true
            }
        );
        assert_eq!(
            formats(ClipPlatform::Windows, false, true),
            Formats {
                hdrop: true,
                virtual_files: false
            }
        );
        for source in [ClipPlatform::Mac, ClipPlatform::Windows] {
            assert_eq!(
                formats(source, false, false),
                Formats {
                    hdrop: false,
                    virtual_files: false
                }
            );
        }
    }
    #[test]
    fn fake_publication_lifecycle_and_retry() {
        #[derive(Default)]
        struct Fake {
            placeholders: usize,
            removals: usize,
            busy: bool,
        }
        let mut fs = Fake {
            placeholders: 3,
            ..Fake::default()
        };
        let mut sweep = |age, consumed, owners, readers| {
            if retire(age, consumed, owners, readers) {
                fs.removals += 1;
                if !fs.busy {
                    fs.placeholders = 0;
                }
            }
            fs.placeholders
        };
        assert_eq!(sweep(1, false, 0, 0), 3);
        assert_eq!(sweep(1, true, 1, 0), 3);
        assert_eq!(sweep(RETENTION_SECS, false, 1, 1), 3);
        assert_eq!(sweep(RETENTION_SECS, false, 1, 0), 0);
        assert_eq!(fs.removals, 1);
        assert!(retire(1, true, 0, 0));
        assert!(!retire(1, false, 0, 0));
        let mut fs = Fake {
            placeholders: 1,
            busy: true,
            ..Fake::default()
        };
        for busy in [true, false] {
            fs.busy = busy;
            if retire(RETENTION_SECS, false, 0, 0) {
                fs.removals += 1;
                if !fs.busy {
                    fs.placeholders = 0;
                }
            }
        }
        assert_eq!((fs.placeholders, fs.removals), (0, 2));
    }
}
