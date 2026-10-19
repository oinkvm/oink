//! Authenticated local client identity, scoped to a blocking IPC dispatch. It
//! identifies prepared UI leases, never a network peer or authorization grant.
use std::cell::Cell;
thread_local! { static CLIENT: Cell<Option<[u8;16]>> = const { Cell::new(None) }; }

pub(crate) fn new_client() -> [u8; 16] {
    let mut client = [0; 16];
    getrandom::fill(&mut client).expect("local client identity entropy");
    client
}
pub(crate) fn current() -> [u8; 16] {
    static LOCAL: std::sync::OnceLock<[u8; 16]> = std::sync::OnceLock::new();
    CLIENT.get().unwrap_or_else(|| *LOCAL.get_or_init(new_client))
}
pub(crate) fn with_client<T>(client: [u8; 16], run: impl FnOnce() -> T) -> T {
    struct Restore(Option<[u8; 16]>);
    impl Drop for Restore {
        fn drop(&mut self) {
            CLIENT.set(self.0);
        }
    }
    let _restore = Restore(CLIENT.replace(Some(client)));
    run()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pooled_dispatch_identity_is_scoped_and_restored() {
        let initial = current();
        with_client([1; 16], || {
            assert_eq!(current(), [1; 16]);
            with_client([2; 16], || assert_eq!(current(), [2; 16]));
            assert_eq!(current(), [1; 16]);
        });
        assert_eq!(current(), initial);
    }
}
