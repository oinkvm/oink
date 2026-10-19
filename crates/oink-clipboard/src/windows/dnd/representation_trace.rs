//! Ordered native-format classes, once at each boundary; no GetData or names in logs.
use super::*;
use crate::model::{Inventory, NativeFormat};
fn class(format: &NativeFormat) -> char {
    match format {
        NativeFormat::WindowsPredefined(1 | 7 | 13) => 'B',
        NativeFormat::WindowsPredefined(2 | 8 | 17) => 'F',
        NativeFormat::WindowsPredefined(15) => 'J',
        NativeFormat::WindowsRegistered(name) => match name.to_ascii_lowercase().as_str() {
            "html format" => 'C',
            "rich text format" => 'D',
            "png" => 'E',
            "jfif" => 'G',
            "uniformresourcelocator" | "uniformresourcelocatorw" => 'H',
            "filegroupdescriptor" | "filegroupdescriptorw" | "filecontents" => 'M',
            _ if crate::formats::is_marker(oink_foundation::clipboard::ClipPlatform::Windows, name) => 'P',
            _ if name.starts_with("Chromium") => 'O',
            _ => 'A',
        },
        _ => 'A',
    }
}
pub(super) fn inventory(inventory: &Inventory, operation: DragOperation, presented: bool) {
    let mut classes = String::new();
    for item in &inventory.items {
        for rep in &item.representations {
            classes.push(class(&rep.format));
        }
        classes.push(';');
    }
    tracing::debug!(target: "oink_clipboard::windows::dnd",
        dnd_hold_sequence = operation.sequence,
        dnd_representation_schema = 1_u64,
        dnd_representation_stage = u64::from(presented),
        dnd_representation_items = inventory.items.len() as u64,
        dnd_representation_complete = true,
        dnd_representation_classes = classes.as_str(),
        "native drag ordered representation classes");
}
pub(super) fn presented(data: &IDataObject, operation: DragOperation) {
    let id = oink_foundation::content::OfferId {
        clip: oink_foundation::content::ClipId {
            origin: oink_foundation::DeviceId([0; 32]),
            seq: 0,
        },
        generation: operation.sequence,
    };
    // Enumeration only; inventory never calls GetData or transfers file bytes.
    if let Ok(offered) = super::super::content::inventory(data, id) {
        inventory(&offered, operation, true);
    }
}
