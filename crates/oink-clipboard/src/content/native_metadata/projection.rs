//! Turn fulfilled promise capabilities into native file items, preserving the
//! source's ordered formats and their representation identities.
use super::*;
use crate::model::{
    ContentItem, FileNode, Inventory, ItemId, Medium, NativeFormat, Representation, RepresentationData,
    RepresentationId,
};

pub(crate) fn inventory(
    mut inventory: Inventory,
    roots: Vec<FileNode>,
    native: &NativeFileMetadata,
) -> io::Result<Inventory> {
    native.validate(&roots.iter().map(|node| node.id).collect::<Vec<_>>())?;
    let mut next_item = inventory.items.iter().map(|item| item.id.0).max().unwrap_or(0);
    let mut next_rep = inventory
        .items
        .iter()
        .flat_map(|item| &item.representations)
        .map(|rep| rep.id.0)
        .max()
        .unwrap_or(0);
    let mut output = Vec::new();
    for mut item in inventory.items {
        let mut anchors = item.representations.iter().filter_map(|rep| match rep.data {
            RepresentationData::Promise(anchor, _) => Some(anchor),
            _ => None,
        });
        let Some(anchor) = anchors.next() else {
            output.push(item);
            continue;
        };
        if anchors.any(|other| other != anchor) {
            return Err(crate::content::unavailable(Unavailable::InvalidData));
        }
        let group = native
            .promises
            .iter()
            .find(|group| group.anchor == anchor)
            .ok_or_else(|| crate::content::unavailable(Unavailable::InvalidData))?;
        for rep in &mut item.representations {
            if let (NativeFormat::Mac(name), RepresentationData::Promise(_, bytes)) = (&rep.format, &mut rep.data)
                && let Some(control) = group.controls.iter().find(|control| &control.name == name)
            {
                *bytes = control.bytes.clone();
            }
        }
        if item.file.is_some() {
            output.push(item);
            continue;
        }
        let first = *group
            .roots
            .first()
            .ok_or_else(|| crate::content::unavailable(Unavailable::InvalidData))?;
        item.file = Some(first);
        item.virtual_files = true;
        item.representations.push(file_representation(first, &mut next_rep)?);
        output.push(item);
        for root in group.roots.iter().skip(1) {
            next_item = next_item
                .checked_add(1)
                .ok_or_else(|| crate::content::unavailable(Unavailable::LimitExceeded))?;
            output.push(ContentItem {
                id: ItemId(next_item),
                file: Some(*root),
                virtual_files: true,
                representations: vec![file_representation(*root, &mut next_rep)?],
            });
        }
    }
    inventory.items = output;
    inventory.roots = roots;
    inventory.validate().map_err(crate::content::unavailable)?;
    Ok(inventory)
}

fn file_representation(root: NodeId, next: &mut u32) -> io::Result<Representation> {
    *next = next
        .checked_add(1)
        .ok_or_else(|| crate::content::unavailable(Unavailable::LimitExceeded))?;
    Ok(Representation {
        id: RepresentationId(*next),
        format: NativeFormat::Mac("public.file-url".into()),
        standard: None,
        medium: Medium::Bytes,
        aspect: 1,
        index: -1,
        data: RepresentationData::File(root),
    })
}
