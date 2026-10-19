//! Native panel policy values, shared by constructors and hermetic tests.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanelPolicy {
    pub nonactivating: bool,
    pub hides_on_deactivate: bool,
    pub can_hide: bool,
}

impl PanelPolicy {
    /// Sharing feedback belongs to the backend and survives application hiding.
    pub const BACKEND_OVERLAY: Self = Self {
        nonactivating: true,
        hides_on_deactivate: false,
        can_hide: false,
    };

    /// The transient UI popover can follow application hiding; outside click and
    /// Escape destroy it. It takes keyboard focus without activating the app.
    pub const WEB_POPOVER: Self = Self {
        nonactivating: true,
        hides_on_deactivate: false,
        can_hide: true,
    };
}
