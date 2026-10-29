//! Reconstruct the original descriptor order over the same received namespace.
//! A FileContents index addresses this table, never the cloud catalog's order.
use super::*;
use crate::{content::native_metadata::NativeFileMetadata, model::FileNode};
use std::{collections::HashMap, io};

pub(crate) fn native_descriptors(
    entries: &[FileEntry],
    nodes: &[FileNode],
    native: &NativeFileMetadata,
) -> io::Result<Descriptors> {
    let invalid = || crate::content::unavailable(crate::model::Unavailable::InvalidData);
    native.validate(
        &nodes
            .iter()
            .filter(|node| node.parent.is_none())
            .map(|node| node.id)
            .collect::<Vec<_>>(),
    )?;
    if nodes.len() != entries.len() {
        return Err(invalid());
    }
    let generated = descriptors(entries);
    if generated.skipped_long != 0 {
        return Err(invalid());
    }
    let by_id: HashMap<_, _> = nodes.iter().enumerate().map(|(index, node)| (node.id, index)).collect();
    let mut original: Vec<_> = native.windows.iter().collect();
    original.sort_by_key(|entry| entry.index);
    let has_ansi = original.iter().all(|entry| entry.ansi.is_some());
    if original.is_empty() || (original.iter().any(|entry| entry.ansi.is_some()) && !has_ansi) {
        return Err(invalid());
    }
    let mut output = Descriptors {
        bytes: (original.len() as u32).to_le_bytes().to_vec(),
        ansi: has_ansi.then(|| (original.len() as u32).to_le_bytes().to_vec()),
        index: Vec::new(),
        meta: Vec::new(),
        skipped_long: 0,
    };
    for entry in original {
        let index = *by_id.get(&entry.node).ok_or_else(invalid)?;
        let mut root = &nodes[index];
        for depth in 0..256 {
            match root.parent {
                Some(parent) if depth < 255 => root = &nodes[*by_id.get(&parent).ok_or_else(invalid)?],
                None if root.id == entry.root => break,
                _ => return Err(invalid()),
            }
        }
        let header = entry.wide.as_ref().or(entry.ansi.as_ref()).ok_or_else(invalid)?;
        let mut wide = generated.bytes[4 + index * FILEDESCRIPTOR_SIZE..4 + (index + 1) * FILEDESCRIPTOR_SIZE].to_vec();
        wide[..72].copy_from_slice(&header[..72]);
        let flags = u32::from_le_bytes(header[..4].try_into().unwrap());
        let mut meta = generated.meta[index].clone();
        if flags & FD_FILESIZE != 0 && !meta.is_dir {
            let size = (u64::from(u32::from_le_bytes(header[64..68].try_into().unwrap())) << 32)
                | u64::from(u32::from_le_bytes(header[68..72].try_into().unwrap()));
            if size != meta.size {
                return Err(crate::content::unavailable(crate::model::Unavailable::Superseded));
            }
        }
        meta.mtime = if flags & FD_WRITESTIME != 0 {
            u64::from_le_bytes(header[56..64].try_into().unwrap())
        } else {
            0
        };
        output.bytes.extend(wide);
        output.index.push(index);
        output.meta.push(meta);
        if let (Some(bytes), Some(ansi)) = (&mut output.ansi, &entry.ansi) {
            let mut ansi = ansi.clone();
            let end = ansi[72..].iter().position(|byte| *byte == 0).ok_or_else(invalid)?;
            let name = super::super::ansi_to_string(&ansi[72..72 + end]);
            if name.is_empty() {
                return Err(invalid());
            }
            let safe = descriptor_path(&name);
            if safe != name {
                let encoded = crate::content::url::ansi(&safe)?;
                if encoded.len() > 260 {
                    return Err(invalid());
                }
                ansi[72..].fill(0);
                ansi[72..72 + encoded.len()].copy_from_slice(&encoded);
            }
            bytes.extend(ansi);
        }
    }
    Ok(output)
}
