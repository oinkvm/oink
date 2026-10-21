//! AppKit renderer shared by the backend status menu and UI-helper Dock menu.
use objc2::rc::Retained;
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{NSMenu, NSMenuItem};
use objc2_foundation::{NSObject, NSObjectProtocol, NSString};

struct Actions {
    ids: Vec<&'static str>,
    invoke: Box<dyn Fn(&str)>,
}
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "OinkNativeControlTarget"]
    #[ivars = Actions]
    struct Target;
    unsafe impl NSObjectProtocol for Target {}
    impl Target {
        #[unsafe(method(invoke:))]
        fn invoke(&self, sender: &NSMenuItem) {
            if let Some(id) = usize::try_from(sender.tag()).ok().and_then(|i| self.ivars().ids.get(i)) {
                (self.ivars().invoke)(id);
            }
        }
    }
);
/// Retains the action target, which AppKit's menu items reference weakly.
pub struct Menu {
    pub menu: Retained<NSMenu>,
    _target: Retained<Target>,
}
impl std::fmt::Debug for Menu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Menu").finish_non_exhaustive()
    }
}
impl Menu {
    pub fn new(model: &super::Model, invoke: impl Fn(&str) + 'static, mtm: MainThreadMarker) -> Self {
        let ids = model
            .groups
            .iter()
            .flat_map(|g| &g.items)
            .chain(&model.tasks)
            .map(|i| i.id)
            .collect();
        let target = Target::alloc(mtm).set_ivars(Actions {
            ids,
            invoke: Box::new(invoke),
        });
        // SAFETY: initializes our freshly allocated NSObject subclass.
        let target: Retained<Target> = unsafe { msg_send![super(target), init] };
        let menu = NSMenu::new(mtm);
        menu.setAutoenablesItems(false);
        let mut tag = 0;
        let mut append = |menu: &NSMenu, item: &super::Item| {
            let native = NSMenuItem::new(mtm);
            native.setTitle(&NSString::from_str(&item.label));
            native.setEnabled(item.enabled);
            native.setState(isize::from(item.checked == Some(true)));
            native.setTag(tag);
            tag += 1;
            // SAFETY: Menu retains target for longer than every item; selector matches.
            unsafe {
                // AppKit may keep an older menu open while facts rebuild the
                // Dock cache. Retain this target for that menu item's lifetime.
                native.setRepresentedObject(Some(&target));
                native.setTarget(Some(&target));
                native.setAction(Some(sel!(invoke:)));
            }
            menu.addItem(&native);
        };
        for group in &model.groups {
            let submenu = NSMenu::new(mtm);
            submenu.setAutoenablesItems(false);
            for item in &group.items {
                append(&submenu, item);
            }
            let parent = NSMenuItem::new(mtm);
            parent.setTitle(&NSString::from_str(&group.label));
            parent.setSubmenu(Some(&submenu));
            menu.addItem(&parent);
        }
        menu.addItem(&NSMenuItem::separatorItem(mtm));
        for item in &model.tasks {
            append(&menu, item);
        }
        Self { menu, _target: target }
    }
}
