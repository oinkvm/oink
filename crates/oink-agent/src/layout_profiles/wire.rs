//! Typed companion DTO conversion; never extends the frozen layout register.
use super::{model::*, types::*};
use crate::api::{DeviceKey, GroupKey, Placement, ScreenKey};
use oink_display::layout::LayoutSettings;
use oink_foundation::{Point, Size};
use oink_protocol::v1::{
    self, RawValue as V, dto as d,
    schema::{WireDto, WireValue},
};

fn map<const N: usize>(v: [(u32, V); N]) -> V {
    V::Map(v.into())
}
fn list<T>(values: impl IntoIterator<Item = T>, f: impl Fn(T) -> V) -> V {
    V::Array(values.into_iter().map(f).collect())
}
fn dev(v: DeviceKey) -> V {
    crate::config::store::wire_device(v.0).write()
}
fn device(v: d::DeviceId) -> v1::Result<DeviceKey> {
    Ok(DeviceKey(oink_foundation::DeviceId(
        v.public_key()?
            .0
            .try_into()
            .map_err(|_| v1::ErrorCode::UnsupportedValue)?,
    )))
}
fn bytes(s: &str, n: usize) -> v1::Result<V> {
    if s.len() != n * 2 {
        return Err(v1::ErrorCode::UnsupportedValue.into());
    }
    let b = (0..s.len())
        .step_by(2)
        .map(|i| {
            s.get(i..i + 2)
                .and_then(|s| u8::from_str_radix(s, 16).ok())
                .ok_or(v1::ErrorCode::UnsupportedValue.into())
        })
        .collect::<v1::Result<Vec<_>>>()?;
    Ok(V::Bytes(b))
}
fn hex(b: &[u8]) -> String {
    b.iter().map(|b| format!("{b:02x}")).collect()
}
fn ver(v: &Version) -> V {
    map([(1, v.counter.write()), (3, dev(v.writer))])
}
fn version(v: d::ConfigVersion) -> v1::Result<Version> {
    Ok(Version {
        counter: v.counter()?,
        writer: device(v.writer()?)?,
    })
}
pub fn point(v: Point) -> V {
    map([(1, v.x.write()), (2, v.y.write())])
}
fn read_point(v: d::Point) -> v1::Result<Point> {
    Ok(Point::new(v.x()?, v.y()?))
}
fn screen(v: &ScreenKey) -> V {
    map([(1, dev(v.device)), (2, V::from(v.display.as_str()))])
}
fn read_screen(v: d::ScreenId) -> v1::Result<ScreenKey> {
    Ok(ScreenKey {
        device: device(v.device()?)?,
        display: v.display()?.0,
    })
}
pub fn native(v: &NativeState) -> v1::Result<d::LayoutProfileNative> {
    d::LayoutProfileNative::from_raw(
        [
            (1, dev(v.device)),
            (2, screen(&v.primary)),
            (
                3,
                list(&v.screens, |s| {
                    map([
                        (1, screen(&s.screen)),
                        (2, point(s.origin)),
                        (3, map([(1, s.size.w.write()), (2, s.size.h.write())])),
                    ])
                }),
            ),
            (4, V::from(v.token.as_str())),
        ]
        .into(),
    )
}
pub fn read_native(v: d::LayoutProfileNative) -> v1::Result<NativeState> {
    Ok(NativeState {
        device: device(v.device()?)?,
        primary: read_screen(v.primary()?)?,
        token: v.token()?.map(|t| t.0).unwrap_or_default(),
        screens: v
            .screens()?
            .0
            .into_iter()
            .map(|s| {
                Ok(NativeScreen {
                    screen: read_screen(s.screen()?)?,
                    origin: read_point(s.origin()?)?,
                    size: Size::new(s.size()?.w()?, s.size()?.h()?),
                })
            })
            .collect::<v1::Result<_>>()?,
    })
}
fn topology(v: &LayoutSettings) -> V {
    map([
        (6, v.gap_travel.write()),
        (7, v.overlap_tolerance.write()),
        (8, v.minimum_portal.write()),
        (9, v.portal_snap.write()),
        (10, v.walls.write()),
        (12, v.endpoint_reach.write()),
    ])
}
pub fn disabled(v: &Disabled) -> V {
    map([(1, screen(&v.screen)), (2, v.disabled.write())])
}
pub fn read_disabled(v: d::LayoutProfileDisabled) -> v1::Result<Disabled> {
    Ok(Disabled {
        screen: read_screen(v.screen()?)?,
        disabled: v.disabled()?,
    })
}
pub(crate) fn snapshot(v: &ArrangementSnapshot) -> v1::Result<V> {
    Ok(map([
        (
            1,
            list(&v.placements, |p| {
                map([(1, dev(p.device)), (2, point(p.origin)), (3, p.scale.write())])
            }),
        ),
        (2, list(&v.disabled, disabled)),
        (
            3,
            V::Array(
                v.native
                    .iter()
                    .map(|n| native(n).map(|n| n.write()))
                    .collect::<v1::Result<_>>()?,
            ),
        ),
        (4, topology(&v.topology)),
        (
            5,
            list(&v.screen_shared, |s| map([(1, dev(s.device)), (2, s.shared.write())])),
        ),
    ]))
}
pub(crate) fn read_snapshot(v: d::LayoutProfileSnapshot, active: &str) -> v1::Result<ArrangementSnapshot> {
    let t = v.topology()?;
    let mut s = ArrangementSnapshot {
        screen_shared: v
            .screen_shared()?
            .map(|list| {
                list.0
                    .into_iter()
                    .map(|d| {
                        Ok(DeviceSharing {
                            device: device(d.device()?)?,
                            shared: d.shared()?,
                        })
                    })
                    .collect::<v1::Result<Vec<_>>>()
            })
            .transpose()?
            .unwrap_or_default(),
        placements: v
            .placements()?
            .0
            .into_iter()
            .map(|p| {
                Ok(Placement {
                    device: device(p.device()?)?,
                    origin: read_point(p.origin()?)?,
                    scale: p.scale()?,
                })
            })
            .collect::<v1::Result<_>>()?,
        disabled: v
            .disabled()?
            .0
            .into_iter()
            .map(read_disabled)
            .collect::<v1::Result<_>>()?,
        native: v.native()?.0.into_iter().map(read_native).collect::<v1::Result<_>>()?,
        topology: LayoutSettings {
            gap_travel: t.gap_travel()?,
            overlap_tolerance: t.overlap_tolerance()?,
            minimum_portal: t.minimum_portal()?,
            portal_snap: t.portal_snap()?,
            walls: t.walls()?,
            endpoint_reach: t.endpoint_reach()?,
        },
        active_profile_id: active.into(),
    };
    canonicalize(&mut s);
    Ok(s)
}
pub fn catalog(v: &Catalog) -> v1::Result<d::LayoutProfileCatalogV3> {
    let profiles = v
        .profiles
        .iter()
        .map(|p| {
            let mut raw = [
                (1, bytes(&p.id, 16)?),
                (3, ver(&p.content_revision)),
                (
                    4,
                    map([
                        (1, p.last_used.logical_ms.write()),
                        (2, dev(p.last_used.writer)),
                        (3, p.last_used.sequence.write()),
                    ]),
                ),
                (5, snapshot(&p.content)?),
                (
                    6,
                    map([
                        (1, list(&p.match_state.online_devices, |d| dev(*d))),
                        (
                            2,
                            list(&p.match_state.connected_screens, |(device, screens)| {
                                let mut raw = [(1, dev(*device))].into_iter().collect::<v1::RawMap>();
                                if let Some(screens) = screens {
                                    raw.insert(2, list(screens, |s| V::from(s.as_str())));
                                }
                                V::Map(raw)
                            }),
                        ),
                    ]),
                ),
            ]
            .into_iter()
            .collect::<v1::RawMap>();
            if let Some(group) = p.default_group {
                raw.insert(7, V::Bytes(group.0.0.to_vec()));
            }
            if let ProfileName::Custom { value } = &p.name {
                raw.insert(2, V::from(value.as_str()));
            }
            Ok(V::Map(restore_extensions(raw, &p.extensions)?))
        })
        .collect::<v1::Result<Vec<_>>>()?;
    d::LayoutProfileCatalogV3::from_raw(restore_extensions(
        [
            (1, v.schema.write()),
            (2, V::Bytes(v.group.0.0.to_vec())),
            (3, ver(&v.version)),
            (4, bytes(&v.active_profile_id, 16)?),
            (5, V::Array(profiles)),
            (
                9,
                V::Array(
                    v.completed_joins
                        .iter()
                        .map(|(id, j)| {
                            let mut raw: v1::RawMap = [
                                (1, bytes(id, 32)?),
                                (2, ver(&j.version)),
                                (
                                    4,
                                    V::Array(
                                        j.dropped_defaults
                                            .iter()
                                            .map(|id| bytes(id, 16))
                                            .collect::<v1::Result<_>>()?,
                                    ),
                                ),
                            ]
                            .into();
                            if let Some(id) = &j.default_profile {
                                raw.insert(3, bytes(id, 16)?);
                            }
                            Ok(V::Map(raw))
                        })
                        .collect::<v1::Result<_>>()?,
                ),
            ),
        ]
        .into(),
        &v.extensions,
    )?)
}
pub fn read_catalog(v: d::LayoutProfileCatalogV3) -> Result<Catalog> {
    fn read(v: d::LayoutProfileCatalogV3) -> v1::Result<Catalog> {
        let active = hex(&v.active()?.0);
        let mut retained =
            extensions(&V::Map(v.raw().clone()), "LayoutProfileCatalogV3").unwrap_or(V::Map(Default::default()));
        if let V::Map(raw) = &mut retained {
            raw.remove(&5);
            for key in [6, 7, 8] {
                raw.remove(&key);
            }
        }
        Ok(Catalog {
            extensions: extension_bytes(retained)?,
            schema: v.schema()?,
            group: GroupKey(oink_foundation::GroupId(v.group()?.0)),
            version: version(v.version()?)?,
            active_profile_id: active.clone(),
            profiles: v
                .profiles()?
                .0
                .into_iter()
                .map(|p| {
                    let used = p.last_used()?;
                    let state = p.state()?;
                    Ok(Profile {
                        extensions: extension_bytes(
                            extensions(&p.write(), "LayoutProfile").unwrap_or(V::Map(Default::default())),
                        )?,
                        id: hex(&p.id()?.0),
                        default_group: p
                            .default_group()?
                            .map(|id| GroupKey(oink_foundation::GroupId(id.0)))
                            .or_else(|| {
                                let group = GroupKey(oink_foundation::GroupId(v.group().ok()?.0));
                                (hex(&p.id().ok()?.0) == default_id(group)).then_some(group)
                            }),
                        name: p
                            .name()?
                            .map(|n| ProfileName::Custom { value: n.0 })
                            .unwrap_or(ProfileName::Default),
                        content_revision: version(p.content_revision()?)?,
                        last_used: Recency {
                            logical_ms: used.logical_ms()?,
                            writer: device(used.writer()?)?,
                            sequence: used.sequence()?,
                        },
                        content: read_snapshot(p.content()?, "")?,
                        match_state: DeviceState {
                            online_devices: state.online()?.0.into_iter().map(device).collect::<v1::Result<_>>()?,
                            connected_screens: state
                                .devices()?
                                .0
                                .into_iter()
                                .map(|d| {
                                    Ok((
                                        device(d.device()?)?,
                                        d.displays()?.map(|v| v.0.into_iter().map(|s| s.0).collect()),
                                    ))
                                })
                                .collect::<v1::Result<_>>()?,
                        },
                    })
                })
                .collect::<v1::Result<_>>()?,
            completed_joins: v
                .completed_joins()?
                .0
                .into_iter()
                .map(|j| {
                    Ok((
                        hex(&j.merge_id()?.0),
                        CompletedJoin {
                            version: version(j.version()?)?,
                            default_profile: j.default_profile()?.map(|id| hex(&id.0)),
                            dropped_defaults: j.dropped_defaults()?.0.into_iter().map(|id| hex(&id.0)).collect(),
                        },
                    ))
                })
                .collect::<v1::Result<_>>()?,
        })
    }
    let c = read(v).map_err(invalid)?;
    c.validate()?;
    Ok(c)
}
pub fn invalid(e: impl std::fmt::Display) -> ProfileError {
    ProfileError::new(ErrorCode::InvalidGeometry, format!("Invalid profile wire data: {e}"))
}

// Keep future numbered fields alongside the native model. Known fields are
// regenerated; unknown subtrees follow stable identities through edits/merges.
fn shape_name(shape: &str) -> String {
    let mut name = shape.replace(' ', "");
    if let Some(inner) = name.strip_prefix("Option<").and_then(|v| v.strip_suffix('>')) {
        name = inner.to_owned();
    }
    if let Some(inner) = name.strip_prefix("List<") {
        name = inner.split(',').next().unwrap_or(inner).to_owned();
    }
    name
}
fn extensions(value: &V, shape: &str) -> Option<V> {
    let name = shape_name(shape);
    let schema = v1::registry::SCHEMAS.iter().find(|s| s.name == name)?;
    match value {
        V::Map(raw) => {
            let mut unknown = v1::RawMap::new();
            for (key, value) in raw {
                if let Some(field) = schema.fields.iter().find(|f| f.key == *key) {
                    if let Some(child) = extensions(value, field.shape) {
                        unknown.insert(*key, child);
                    }
                } else {
                    unknown.insert(*key, value.clone());
                }
            }
            (!unknown.is_empty()).then_some(V::Map(unknown))
        }
        V::Array(values) => {
            let unknown = values
                .iter()
                .filter_map(|v| {
                    let mut ext = extensions(v, &name)?;
                    if let (V::Map(old), V::Map(raw)) = (v, &mut ext)
                        && let Some(id) = old.get(&1)
                    {
                        raw.insert(1, id.clone());
                    }
                    Some(ext)
                })
                .collect::<Vec<_>>();
            (!unknown.is_empty()).then_some(V::Array(unknown))
        }
        _ => None,
    }
}
fn extension_bytes(value: V) -> v1::Result<Vec<u8>> {
    let raw = value.as_map()?;
    if raw.is_empty() {
        Ok(Vec::new())
    } else {
        v1::cbor::encode_map(raw, MAX_BYTES)
    }
}
fn restore_extensions(raw: v1::RawMap, bytes: &[u8]) -> v1::Result<v1::RawMap> {
    if bytes.is_empty() {
        return Ok(raw);
    }
    fn overlay(current: &mut V, extra: V) {
        match (current, extra) {
            (V::Map(current), V::Map(extra)) => {
                for (key, value) in extra {
                    if let Some(known) = current.get_mut(&key) {
                        overlay(known, value);
                    } else {
                        current.insert(key, value);
                    }
                }
            }
            (V::Array(current), V::Array(extra)) => {
                for value in extra {
                    let id = value.as_map().ok().and_then(|m| m.get(&1));
                    if let Some(known) = current
                        .iter_mut()
                        .find(|v| id.is_some() && v.as_map().ok().and_then(|m| m.get(&1)) == id)
                    {
                        overlay(known, value);
                    }
                }
            }
            _ => {}
        }
    }
    let mut value = V::Map(raw);
    overlay(&mut value, V::Map(v1::cbor::decode_map(bytes, MAX_BYTES)?));
    Ok(value.as_map()?.clone())
}
