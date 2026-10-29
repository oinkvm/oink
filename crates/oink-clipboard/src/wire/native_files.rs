use super::*;
use crate::content::native_metadata::{MacPromiseControl, MacPromiseGroup, NativeFileMetadata, WindowsFileDescriptor};

pub(super) fn native_files(value: &NativeFileMetadata) -> V {
    if value.is_empty() {
        return V::Null;
    }
    V::Map(map([
        (
            1,
            V::Array(
                value
                    .windows
                    .iter()
                    .map(|entry| {
                        V::Map(map([
                            (1, number(entry.root.0)),
                            (2, number(entry.node.0)),
                            (3, number(entry.index)),
                            (4, entry.wide.as_ref().map_or(V::Null, |bytes| V::Bytes(bytes.clone()))),
                            (5, entry.ansi.as_ref().map_or(V::Null, |bytes| V::Bytes(bytes.clone()))),
                        ]))
                    })
                    .collect(),
            ),
        ),
        (
            2,
            V::Array(
                value
                    .promises
                    .iter()
                    .map(|group| {
                        V::Map(map([
                            (1, number(group.anchor.0)),
                            (2, V::Array(group.roots.iter().map(|id| number(id.0)).collect())),
                            (3, V::Array(group.indices.iter().map(|index| number(*index)).collect())),
                            (
                                4,
                                if group.controls.is_empty() {
                                    V::Null
                                } else {
                                    V::Array(
                                        group
                                            .controls
                                            .iter()
                                            .map(|control| {
                                                V::Map(map([
                                                    (1, V::Text(control.name.clone())),
                                                    (2, V::Bytes(control.bytes.clone())),
                                                ]))
                                            })
                                            .collect(),
                                    )
                                },
                            ),
                        ]))
                    })
                    .collect(),
            ),
        ),
    ]))
}

pub(super) fn local_native_files(value: Option<wire::NativeFileMetadata>) -> v1::Result<NativeFileMetadata> {
    let Some(value) = value else {
        return Ok(NativeFileMetadata::default());
    };
    Ok(NativeFileMetadata {
        windows: value
            .windows()?
            .0
            .into_iter()
            .map(|entry| {
                Ok(WindowsFileDescriptor {
                    root: domain::NodeId(entry.root()?),
                    node: domain::NodeId(entry.node()?),
                    index: entry.index()?,
                    wide: entry.wide()?.map(|bytes| bytes.0),
                    ansi: entry.ansi()?.map(|bytes| bytes.0),
                })
            })
            .collect::<v1::Result<_>>()?,
        promises: value
            .promises()?
            .0
            .into_iter()
            .map(|group| {
                Ok(MacPromiseGroup {
                    controls: group
                        .controls()?
                        .map_or_else(Vec::new, |controls| controls.0)
                        .into_iter()
                        .map(|control| {
                            Ok(MacPromiseControl {
                                name: control.name()?.0,
                                bytes: control.bytes()?.0,
                            })
                        })
                        .collect::<v1::Result<_>>()?,
                    anchor: domain::NodeId(group.anchor()?),
                    roots: group.roots()?.0.into_iter().map(domain::NodeId).collect(),
                    indices: group.indices()?.0,
                })
            })
            .collect::<v1::Result<_>>()?,
    })
}
