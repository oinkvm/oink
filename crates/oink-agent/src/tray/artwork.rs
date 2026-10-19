//! The 16×16 pointer, tail and badge paths used by the designer tray artwork.
use super::{TrayLook, TrayState};

#[derive(Clone, Copy, Debug)]
pub enum TrayPaint {
    MacTemplate,
    Windows { dark: bool, accent: [u8; 3] },
}

pub fn svg(look: TrayLook, paint: TrayPaint) -> String {
    let (mac, pointer, tail, badge): (bool, String, String, String) = match paint {
        TrayPaint::MacTemplate => (true, "#000000".into(), "#000000".into(), "#000000".into()),
        TrayPaint::Windows {
            dark,
            accent: [r, g, b],
        } => {
            let accent = format!("#{r:02X}{g:02X}{b:02X}");
            let badge = if matches!(look.state, TrayState::Paused | TrayState::ScreenOff) {
                "#F04452".into()
            } else {
                accent.clone()
            };
            (false, if dark { "#F2F3F5" } else { "#1B1E28" }.into(), accent, badge)
        }
    };
    let (area, tail_path, pointer_path) = if mac {
        (
            r#"<path d="M1 1L1 10.4L8.4 7.6Z"/>"#.to_owned(),
            format!(
                r#"<path d="M4.5 9C4.5 12.3 7.8 14 12 14A2 2 0 1 0 10 12" fill="none" stroke="{tail}" stroke-width="2" stroke-linecap="round"/>"#
            ),
            format!(
                r#"<path d="M1 1L1 10.4L8.4 7.6Z" fill="{}" stroke="{pointer}" stroke-width="{}" stroke-linejoin="round"/>"#,
                if look.solid { &pointer } else { "none" },
                if look.solid { "1" } else { "1.3" }
            ),
        )
    } else {
        let path = "M9 6V39L33.5 29.5Q23.5 15.5 9 6Z";
        (
            format!(r#"<path transform="scale(.25) translate(0 1.5)" d="{path}"/>"#),
            format!(
                r#"<path transform="scale(.25) translate(0 1.5)" d="M20.5 32C20.5 46 31 54.5 45 54.5A9.5 9.5 0 1 0 36.3 41.2" fill="none" stroke="{tail}" stroke-width="5.5" stroke-linecap="round"/>"#
            ),
            format!(
                r#"<path transform="scale(.25) translate(0 1.5)" d="{path}" fill="{}" stroke="{pointer}" stroke-width="5" stroke-linejoin="round"/>"#,
                if look.solid { &pointer } else { "none" }
            ),
        )
    };
    let badge_mask = match look.state {
        TrayState::Locked => {
            r##"<g transform="translate(8.75 -0.45) scale(.55)" fill="#fff"><path d="M4.3 6.6V4.6a2.7 2.7 0 0 1 5.4 0v2" fill="none" stroke="#fff" stroke-width="1.9" stroke-linecap="round"/><rect x="2.4" y="6.2" width="9.2" height="7" rx="1.8"/><circle cx="7" cy="9.7" r="1.15" fill="#000"/></g>"##
        }
        TrayState::Game => {
            r##"<g transform="translate(8.75 -0.5) scale(.47)" fill="#fff"><path d="M5.1 4.3h5.8a3.6 3.6 0 0 1 3.5 2.8l.9 4.1a1.9 1.9 0 0 1-3.2 1.7l-1.6-1.8H5.5l-1.6 1.8a1.9 1.9 0 0 1-3.2-1.7l.9-4.1a3.6 3.6 0 0 1 3.5-2.8z" stroke="#fff" stroke-width="1" stroke-linejoin="round"/><path d="M4.7 6.9v2.2M3.6 8h2.2" fill="none" stroke="#000" stroke-width="1.5" stroke-linecap="round"/><g fill="#000"><circle cx="10.9" cy="7.3" r="1"/><circle cx="12.3" cy="8.8" r="1"/></g></g>"##
        }
        TrayState::Paused => {
            r##"<g fill="#fff"><rect x="10" y="0.6" width="2" height="6" rx=".7"/><rect x="13.4" y="0.6" width="2" height="6" rx=".7"/></g>"##
        }
        TrayState::ScreenOff => {
            r##"<g fill="none" stroke="#fff" stroke-width="1.4" stroke-linecap="round"><circle cx="12.6" cy="3.6" r="3.1"/><path d="M10.4 1.4l4.4 4.4"/></g>"##
        }
        TrayState::Default | TrayState::Cursor => "",
    };
    let mark = if look.solid {
        format!("{tail_path}{pointer_path}")
    } else {
        format!(
            r##"<defs><mask id="tail" maskUnits="userSpaceOnUse" x="0" y="0" width="16" height="16"><rect width="16" height="16" fill="#fff"/><g fill="#000">{area}</g></mask></defs><g mask="url(#tail)">{tail_path}</g>{pointer_path}"##
        )
    };
    let content = if badge_mask.is_empty() {
        mark
    } else {
        format!(
            r##"<defs><mask id="knock" maskUnits="userSpaceOnUse" x="0" y="0" width="16" height="16"><rect width="16" height="16" fill="#fff"/><circle cx="12.6" cy="3.6" r="4.6" fill="#000"/></mask><mask id="badge" maskUnits="userSpaceOnUse" x="0" y="0" width="16" height="16">{badge_mask}</mask></defs><g mask="url(#knock)">{mark}</g><rect width="16" height="16" fill="{badge}" mask="url(#badge)"/>"##
        )
    };
    format!(r#"<svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" viewBox="0 0 16 16">{content}</svg>"#)
}
