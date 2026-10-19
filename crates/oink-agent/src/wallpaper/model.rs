//! In-memory JPEG thumbnails. Debug output never includes display IDs or image data.
use oink_protocol::v1::{
    self, Message, RawValue as V, dto,
    schema::{WireDto, WireValue},
};
pub(super) const FORMAT: &str = "mime:image/jpeg";
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ScreenWallpaper {
    pub display: String,
    pub width: u16,
    pub height: u16,
    pub jpeg: Vec<u8>,
}
impl std::fmt::Debug for ScreenWallpaper {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "ScreenWallpaper({}x{}, {} B)",
            self.width,
            self.height,
            self.jpeg.len()
        )
    }
}
pub(crate) fn encode(revision: u64, screens: &[ScreenWallpaper]) -> v1::Result<dto::WallpaperSet> {
    let screens = screens
        .iter()
        .map(|s| {
            if !s.jpeg.starts_with(&[0xff, 0xd8]) {
                return Err(v1::ErrorCode::UnsupportedValue.into());
            }
            dto::ScreenWallpaper::from_raw(
                [
                    (1, V::Text(s.display.clone())),
                    (2, s.width.write()),
                    (3, s.height.write()),
                    (4, V::Bytes(s.jpeg.clone())),
                ]
                .into(),
            )
            .map(|v| v.write())
        })
        .collect::<v1::Result<Vec<_>>>()?;
    dto::WallpaperSet::from_raw([(1, revision.write()), (2, V::Array(screens)), (3, V::from(FORMAT))].into())
}
pub(crate) fn decode(message: &dto::WallpaperSet) -> v1::Result<Vec<ScreenWallpaper>> {
    if message.format()?.0 != FORMAT {
        return Err(v1::ErrorCode::UnsupportedValue.into());
    }
    message
        .screens()?
        .0
        .into_iter()
        .map(|s| {
            let jpeg = s.bytes()?.0;
            if !jpeg.starts_with(&[0xff, 0xd8]) {
                return Err(v1::ErrorCode::UnsupportedValue.into());
            }
            Ok(ScreenWallpaper {
                display: s.display()?.0,
                width: s.width()?,
                height: s.height()?,
                jpeg,
            })
        })
        .collect()
}
pub(crate) fn message(value: dto::WallpaperSet) -> Message {
    Message::WallpaperSet(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_format_is_explicit_and_debug_does_not_expose_content() {
        let screens = vec![ScreenWallpaper {
            display: "private-display".into(),
            width: 2,
            height: 3,
            jpeg: vec![0xff, 0xd8, 11, 12],
        }];
        let wire = encode(8, &screens).unwrap();
        assert_eq!(decode(&wire).unwrap(), screens);
        assert_eq!(wire.revision().unwrap(), 8);
        assert!(!format!("{:?}", screens[0]).contains("private"));
        assert!(!format!("{:?}", screens[0]).contains("255"));
        let mut future = wire;
        future
            .edit(|raw| {
                raw.insert(3, V::from("mime:image/unknown"));
            })
            .unwrap();
        assert_eq!(decode(&future).unwrap_err().code, v1::ErrorCode::UnsupportedValue);
        let mut invalid = screens;
        invalid[0].jpeg = vec![1, 2];
        assert_eq!(encode(9, &invalid).unwrap_err().code, v1::ErrorCode::UnsupportedValue);
    }
}
