//! Build-provenance projection; native runtime labels are never report fields.
use crate::*;
use serde::{Deserialize, Serialize};
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Image {
    pub slot: ImageSlot,
    pub debug_id: [u8; 16],
    pub debug_age: u32,
    pub code_id: Option<u64>,
    pub text_size: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: AppVersion,
    pub images: Vec<Image>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackagedManifest {
    schema: u16,
    source_tree_sha256: String,
    version: AppVersion,
    images: Vec<Image>,
}
impl Manifest {
    /// Load post-link image metadata from the packaged, fixed resource location.
    /// Image identifiers are produced after linking, so embedding this JSON would
    /// cause a relink cycle. The expected release and source tree come from code.
    pub fn load(executable: &std::path::Path, expected: AppVersion, source_tree_sha256: &str) -> Option<Self> {
        use std::io::Read;
        if source_tree_sha256.len() != 64
            || !source_tree_sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        let path = Self::resource_path(executable)?;
        let mut bytes = Vec::new();
        std::fs::File::open(path)
            .ok()?
            .take(8193)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() > 8192 {
            return None;
        }
        let value: PackagedManifest = serde_json::from_slice(&bytes).ok()?;
        if value.schema != 1
            || value.source_tree_sha256 != source_tree_sha256
            || value.version != expected
            || value.images.is_empty()
            || value.images.len() > 4
            || value.images.iter().any(|i| i.debug_id == [0; 16] || i.text_size == 0)
            || value.images.iter().enumerate().any(|(index, image)| {
                value.images[..index]
                    .iter()
                    .any(|old| old.slot == image.slot && old.debug_id == image.debug_id)
            })
            || value.images.iter().any(|image| {
                value.images.iter().filter(|other| other.slot == image.slot).count()
                    > if image.slot == ImageSlot::Agent { 2 } else { 1 }
            })
        {
            return None;
        }
        Some(Self {
            version: value.version,
            images: value.images,
        })
    }
    /// A shared executable can serve different roles on Windows. Prefer the
    /// armed component when its identifier appears under more than one role.
    pub fn image(&self, debug_id: [u8; 16], preferred: ImageSlot) -> Option<&Image> {
        self.images
            .iter()
            .find(|i| i.debug_id == debug_id && i.slot == preferred)
            .or_else(|| self.images.iter().find(|i| i.debug_id == debug_id))
    }
    fn resource_path(executable: &std::path::Path) -> Option<std::path::PathBuf> {
        #[cfg(target_os = "macos")]
        {
            Some(executable.parent()?.parent()?.join("Resources/report-manifest.json"))
        }
        #[cfg(not(target_os = "macos"))]
        {
            Some(executable.parent()?.join("report-manifest.json"))
        }
    }
    pub fn project(&self, crash: &mut SafeCrash) -> Result<()> {
        if crash.build.app_version != self.version {
            return Err(Error::Invalid);
        }
        for frame in crash.frames.iter_mut().flatten() {
            if let Some(Frame::Native {
                image,
                offset,
                debug_id,
                debug_age,
                code_id,
                image_size,
            }) = frame
                && !self.images.iter().any(|i| {
                    i.slot == *image
                        && i.debug_id == *debug_id
                        && i.debug_age == *debug_age
                        && i.code_id == *code_id
                        && i.text_size == *image_size
                        && *offset < i.text_size
                })
            {
                *frame = None;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn packaged_manifest_binds_actual_images_to_compiled_release_and_source_tree() {
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("Contents/MacOS/oink");
        let path = Manifest::resource_path(&executable).unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let expected = AppVersion {
            major: 0,
            minor: 1,
            patch: 0,
            build: 7,
        };
        let tree = "a".repeat(64);
        let valid = serde_json::json!({"schema":1,"source_tree_sha256":tree,"version":expected,"images":[{"slot":"agent","debug_id":vec![1u8;16],"debug_age":0,"code_id":null,"text_size":4096}]});
        std::fs::write(&path, serde_json::to_vec(&valid).unwrap()).unwrap();
        let manifest = Manifest::load(&executable, expected, &tree).unwrap();
        assert_eq!(manifest.images[0].debug_id, [1; 16]);
        let mut complete = valid.clone();
        complete["images"] = serde_json::json!([
            {"slot":"agent","debug_id":vec![1u8;16],"debug_age":0,"code_id":null,"text_size":4096},
            {"slot":"agent","debug_id":vec![2u8;16],"debug_age":0,"code_id":null,"text_size":4096},
            {"slot":"ui_rust","debug_id":vec![1u8;16],"debug_age":0,"code_id":null,"text_size":4096},
            {"slot":"file_provider","debug_id":vec![3u8;16],"debug_age":0,"code_id":null,"text_size":4096}
        ]);
        std::fs::write(&path, serde_json::to_vec(&complete).unwrap()).unwrap();
        let manifest = Manifest::load(&executable, expected, &tree).unwrap();
        assert_eq!(
            manifest.image([1; 16], ImageSlot::UiRust).unwrap().slot,
            ImageSlot::UiRust
        );
        for slot in ["agent", "ui_rust", "file_provider"] {
            let mut invalid = complete.clone();
            invalid["images"].as_array_mut().unwrap().push(
                serde_json::json!({"slot":slot,"debug_id":vec![4u8;16],"debug_age":0,"code_id":null,"text_size":4096}),
            );
            std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(Manifest::load(&executable, expected, &tree).is_none());
        }
        std::fs::write(&path, serde_json::to_vec(&valid).unwrap()).unwrap();
        assert!(Manifest::load(&executable, AppVersion { build: 8, ..expected }, &tree).is_none());
        assert!(Manifest::load(&executable, expected, &"b".repeat(64)).is_none());
        for (key, value) in [
            ("schema", serde_json::json!(2)),
            ("unknown", serde_json::json!(true)),
            ("images", serde_json::json!([])),
        ] {
            let mut invalid = valid.clone();
            invalid[key] = value;
            std::fs::write(&path, serde_json::to_vec(&invalid).unwrap()).unwrap();
            assert!(Manifest::load(&executable, expected, &tree).is_none());
        }
        let mut duplicate = valid.clone();
        duplicate["images"]
            .as_array_mut()
            .unwrap()
            .push(valid["images"][0].clone());
        std::fs::write(&path, serde_json::to_vec(&duplicate).unwrap()).unwrap();
        assert!(Manifest::load(&executable, expected, &tree).is_none());
        for bytes in [vec![b' '; 8193], b"{broken".to_vec()] {
            std::fs::write(&path, bytes).unwrap();
            assert!(Manifest::load(&executable, expected, &tree).is_none());
        }
    }
}
