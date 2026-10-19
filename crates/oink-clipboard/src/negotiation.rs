//! Reconstruction contracts supported by the native clipboard adapters.
use oink_protocol::v1::{RawValue, dto, schema::WireDto};

pub const PORTABLE_FORMATS: &[&str] = &[
    "mime:text/plain;charset=utf-8",
    "mime:text/html;charset=utf-8",
    "mime:application/rtf",
    "mime:image/png",
    "oink:file-namespace/1",
    "oink:privacy-markers/1",
    dto::URL_FORMAT,
];
pub const CAPABILITIES: &[&str] = &["oink.clipboard/1", "oink.drag/1", "oink.cache/1"];

/// Negotiation describes implemented reconstruction contracts. The per-device
/// filters separately remove disabled representations from every actual offer.
pub fn formats() -> dto::FormatSupport {
    let portable = RawValue::Array(
        PORTABLE_FORMATS
            .iter()
            .map(|name| RawValue::Text((*name).into()))
            .collect(),
    );
    let namespaces = if cfg!(target_os = "macos") {
        vec!["mac-uti"]
    } else if cfg!(windows) {
        vec!["win-name", "win-cf"]
    } else {
        vec![]
    };
    let media = if cfg!(windows) { 0..9 } else { 0..1 };
    dto::FormatSupport::from_raw(
        [
            (1, portable.clone()),
            (2, portable),
            (
                3,
                RawValue::Array(namespaces.into_iter().map(|name| RawValue::Text(name.into())).collect()),
            ),
            (4, RawValue::Array(media.map(RawValue::Uint).collect())),
        ]
        .into(),
    )
    .expect("constant clipboard reconstruction profile")
}

pub fn native_format(format: &oink_foundation::content::NativeFormat) -> String {
    use oink_foundation::content::NativeFormat;
    match format {
        NativeFormat::Mac(name) => format!("mac-uti:{name}"),
        NativeFormat::WindowsRegistered(name) => format!("win-name:{name}"),
        NativeFormat::WindowsPredefined(id) => format!("win-cf:{id}"),
        NativeFormat::Unknown(name) => name.clone(),
    }
}
pub fn portable_format(format: oink_foundation::content::StandardFormat) -> &'static str {
    use oink_foundation::content::StandardFormat;
    PORTABLE_FORMATS[match format {
        StandardFormat::Text => 0,
        StandardFormat::Html => 1,
        StandardFormat::Rtf => 2,
        StandardFormat::Png => 3,
        StandardFormat::Privacy => 5,
        StandardFormat::Url => 6,
    }]
}
/// Select an offered stable item/representation and its reconstruction contract.
pub fn selection(
    inventory: &crate::model::Inventory,
    representation: oink_foundation::content::RepresentationId,
    target: oink_foundation::clipboard::ClipPlatform,
) -> Option<(oink_foundation::content::ItemId, String)> {
    let item = inventory
        .items
        .iter()
        .find(|item| item.representations.iter().any(|rep| rep.id == representation))?;
    let representation = item.representations.iter().find(|rep| rep.id == representation)?;
    let format = if inventory.platform == target {
        native_format(&representation.format)
    } else {
        portable_format(representation.standard?).into()
    };
    Some((item.id, format))
}

fn medium(value: oink_foundation::content::Medium) -> dto::Medium {
    use oink_foundation::content::Medium as M;
    match value {
        M::Bytes => dto::Medium::Bytes,
        M::HGlobal => dto::Medium::HGlobal,
        M::Stream => dto::Medium::Stream,
        M::Storage => dto::Medium::Storage,
        M::Bitmap => dto::Medium::Bitmap,
        M::Palette => dto::Medium::Palette,
        M::Metafile => dto::Medium::Metafile,
        M::EnhancedMetafile => dto::Medium::EnhancedMetafile,
        M::File => dto::Medium::File,
    }
}
/// Choose only a contract both logical endpoints advertised. A relay hop's
/// profile cannot authorise decoding at a different final destination.
pub fn negotiated_selection(
    source: &dto::FormatSupport,
    target_profile: &dto::FormatSupport,
    inventory: &crate::model::Inventory,
    representation: oink_foundation::content::RepresentationId,
    target: oink_foundation::clipboard::ClipPlatform,
) -> oink_protocol::v1::Result<(oink_foundation::content::ItemId, String)> {
    use oink_protocol::v1::ErrorCode;
    let (item, format) = selection(inventory, representation, target).ok_or(ErrorCode::UnsupportedValue)?;
    if inventory.platform == target {
        let representation = crate::content::find(inventory, representation).ok_or(ErrorCode::UnsupportedValue)?;
        let namespace = format.split_once(':').ok_or(ErrorCode::UnsupportedValue)?.0;
        if !source.native_namespaces()?.0.iter().any(|name| name.0 == namespace)
            || !target_profile
                .native_namespaces()?
                .0
                .iter()
                .any(|name| name.0 == namespace)
            || !source.media()?.0.contains(&medium(representation.medium))
            || !target_profile.media()?.0.contains(&medium(representation.medium))
        {
            return Err(ErrorCode::UnsupportedValue.into());
        }
    } else if !source.portable_send()?.0.iter().any(|name| name.0 == format)
        || !target_profile.portable_receive()?.0.iter().any(|name| name.0 == format)
    {
        return Err(ErrorCode::UnsupportedValue.into());
    }
    Ok((item, format))
}

#[cfg(all(test, debug_assertions))]
mod tests {
    use super::*;
    use crate::content::Provider;
    use oink_foundation::{clipboard::ClipPlatform, content::RepresentationId};
    fn profile(portable: &[&str], namespaces: &[&str], media: &[u64]) -> dto::FormatSupport {
        let strings = |names: &[&str]| RawValue::Array(names.iter().map(|s| RawValue::Text((*s).into())).collect());
        dto::FormatSupport::from_raw(
            [
                (1, strings(portable)),
                (2, strings(portable)),
                (3, strings(namespaces)),
                (4, RawValue::Array(media.iter().map(|n| RawValue::Uint(*n)).collect())),
            ]
            .into(),
        )
        .unwrap()
    }
    #[test]
    fn both_endpoint_profiles_must_authorize_the_exact_reconstruction_contract() {
        let inventory = crate::content::tests::fixture().inventory();
        let mac = profile(PORTABLE_FORMATS, &["mac-uti"], &[0]);
        assert!(namespace_supported(&mac, &mac));
        assert!(!namespace_supported(&mac, &profile(&[], &["mac-uti"], &[0])));
        let win = profile(PORTABLE_FORMATS, &["win-name", "win-cf"], &[0, 1, 2]);
        assert!(negotiated_selection(&mac, &mac, &inventory, RepresentationId(2), ClipPlatform::Mac).is_ok());
        assert!(negotiated_selection(&mac, &win, &inventory, RepresentationId(2), ClipPlatform::Mac).is_err());
        let no_media = profile(PORTABLE_FORMATS, &["mac-uti"], &[]);
        assert!(negotiated_selection(&mac, &no_media, &inventory, RepresentationId(2), ClipPlatform::Mac).is_err());
        assert!(negotiated_selection(&mac, &win, &inventory, RepresentationId(1), ClipPlatform::Windows).is_ok());
        assert!(negotiated_selection(&mac, &win, &inventory, RepresentationId(2), ClipPlatform::Windows).is_err());
        let no_text = profile(&["mime:image/png"], &["win-name"], &[0]);
        assert!(negotiated_selection(&mac, &no_text, &inventory, RepresentationId(1), ClipPlatform::Windows).is_err());
        assert!(negotiated_selection(&no_text, &win, &inventory, RepresentationId(1), ClipPlatform::Windows).is_err());
    }
}

/// Namespace transfer is itself a portable contract, including native promises.
pub fn namespace_supported(source: &dto::FormatSupport, target: &dto::FormatSupport) -> bool {
    source
        .portable_send()
        .is_ok_and(|formats| formats.0.iter().any(|f| f.0 == "oink:file-namespace/1"))
        && target
            .portable_receive()
            .is_ok_and(|formats| formats.0.iter().any(|f| f.0 == "oink:file-namespace/1"))
}
