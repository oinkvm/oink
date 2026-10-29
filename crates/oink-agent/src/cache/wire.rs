//! Typed cache counters and the supported UI category projection.
use oink_clipboard::cache::{CacheCategory, CacheClear, CacheSize, CacheUsage};
use oink_foundation::session::Boot;
use oink_protocol::v1::{
    self, RawValue as V, dto,
    schema::{WireDto, WireValue},
};
pub(super) fn boot(value: Boot) -> dto::Boot {
    dto::Boot::from_raw(
        [
            (1, oink_clipboard::wire::device(value.device_id).write()),
            (2, V::Bytes(value.nonce.to_vec())),
        ]
        .into(),
    )
    .expect("fixed boot identity")
}
fn category(value: CacheCategory) -> u64 {
    match value {
        CacheCategory::ClipboardFiles => 0,
        CacheCategory::DragFiles => 1,
        CacheCategory::ProviderCatalog => 2,
        CacheCategory::ProviderMaterialized => 3,
        CacheCategory::ProviderTemporary => 4,
        CacheCategory::DeliveredFiles => 5,
        CacheCategory::SourceSnapshots => 6,
        CacheCategory::PromiseExports => 7,
        CacheCategory::Other => unreachable!("UI-only aggregate"),
    }
}
fn local_category(value: u32) -> CacheCategory {
    match value {
        0 => CacheCategory::ClipboardFiles,
        1 => CacheCategory::DragFiles,
        2 => CacheCategory::ProviderCatalog,
        3 => CacheCategory::ProviderMaterialized,
        4 => CacheCategory::ProviderTemporary,
        5 => CacheCategory::DeliveredFiles,
        6 => CacheCategory::SourceSnapshots,
        7 => CacheCategory::PromiseExports,
        8 => CacheCategory::Other,
        _ => CacheCategory::Other,
    }
}
pub(super) fn project(wire: &dto::CacheUsageValue) -> v1::Result<CacheUsage> {
    let mut usage = CacheUsage {
        categories: vec![],
        total_bytes: wire.total_bytes()?,
        incomplete: wire.incomplete()?,
    };
    let mut total = 0u64;
    for size in wire.categories()?.0 {
        let category = local_category(size.category()?.number());
        let bytes = size.bytes()?;
        let kept_bytes = size.kept_bytes()?;
        if kept_bytes > bytes {
            return Err(v1::ErrorCode::MalformedFrame.into());
        }
        total = total.checked_add(bytes).ok_or(v1::ErrorCode::LimitExceeded)?;
        if let Some(old) = usage.categories.iter_mut().find(|v| v.category == category) {
            old.bytes = old.bytes.checked_add(bytes).ok_or(v1::ErrorCode::LimitExceeded)?;
            old.kept_bytes = old
                .kept_bytes
                .checked_add(kept_bytes)
                .ok_or(v1::ErrorCode::LimitExceeded)?;
        } else {
            usage.categories.push(CacheSize {
                category,
                bytes,
                kept_bytes,
            });
        }
    }
    if total != usage.total_bytes {
        return Err(v1::ErrorCode::MalformedFrame.into());
    }
    Ok(usage)
}
pub(super) fn wire_usage(value: &CacheUsage) -> v1::Result<dto::CacheUsageValue> {
    if value
        .categories
        .iter()
        .any(|size| size.category == CacheCategory::Other)
    {
        return Err(v1::ErrorCode::UnsupportedValue.into());
    }
    let sizes = value
        .categories
        .iter()
        .map(|s| {
            dto::CacheSize::from_raw(
                [
                    (1, V::Uint(category(s.category))),
                    (2, V::Uint(s.bytes)),
                    (3, V::Uint(s.kept_bytes)),
                ]
                .into(),
            )
            .map(|s| s.write())
        })
        .collect::<v1::Result<Vec<_>>>()?;
    dto::CacheUsageValue::from_raw(
        [
            (1, V::Array(sizes)),
            (2, V::Uint(value.total_bytes)),
            (3, V::Bool(value.incomplete)),
        ]
        .into(),
    )
}
pub(super) fn wire_clear(value: &CacheClear) -> v1::Result<dto::CacheClear> {
    dto::CacheClear::from_raw(
        [
            (1, V::Uint(value.removed_bytes)),
            (2, V::Uint(value.kept_bytes)),
            (3, wire_usage(&value.usage)?.write()),
        ]
        .into(),
    )
}
pub(super) fn clear_projection(value: &dto::CacheClear) -> v1::Result<CacheClear> {
    Ok(CacheClear {
        removed_bytes: value.removed_bytes()?,
        kept_bytes: value.kept_bytes()?,
        usage: project(&value.usage()?)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_categories_are_in_other_and_total_without_losing_the_retained_tree() {
        let size = |category, bytes, kept| {
            dto::CacheSize::from_raw(
                [
                    (1, V::Uint(category)),
                    (2, V::Uint(bytes)),
                    (3, V::Uint(kept)),
                    (90, V::Text("future counter".into())),
                ]
                .into(),
            )
            .unwrap()
            .write()
        };
        let wire = dto::CacheUsageValue::from_raw(
            [
                (1, V::Array(vec![size(0, 10, 1), size(1234, 20, 2), size(1235, 30, 3)])),
                (2, V::Uint(60)),
                (3, V::Bool(false)),
                (91, V::Bytes(vec![1, 2])),
            ]
            .into(),
        )
        .unwrap();
        let original = wire.clone();
        let usage = project(&wire).unwrap();
        assert_eq!(usage.total_bytes, 60);
        assert_eq!(usage.categories.len(), 2);
        assert_eq!(
            usage.categories[1],
            CacheSize {
                category: CacheCategory::Other,
                bytes: 50,
                kept_bytes: 5
            }
        );
        assert_eq!(wire, original);
        assert!(
            wire_usage(&usage).is_err(),
            "a supported UI aggregate cannot replace its raw owner tree"
        );
    }
    #[test]
    fn accounting_rejects_inconsistent_totals_and_known_categories_roundtrip() {
        let usage = CacheUsage {
            categories: vec![CacheSize {
                category: CacheCategory::PromiseExports,
                bytes: 100,
                kept_bytes: 30,
            }],
            total_bytes: 100,
            incomplete: true,
        };
        let wire = wire_usage(&usage).unwrap();
        assert_eq!(project(&wire).unwrap(), usage);
        let mut bad = wire.into_raw();
        bad.insert(2, V::Uint(99));
        assert!(project(&dto::CacheUsageValue::from_raw(bad).unwrap()).is_err());
    }
}
