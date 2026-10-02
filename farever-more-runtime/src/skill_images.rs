//! Lazy, bounded loading of game-authored skill icons from Farever's atlases.

use std::collections::{HashMap, HashSet};
use std::io::Cursor;
use std::sync::Arc;

use farever_db::{pak::HeapsPak, GameInstall, IconCrop};
use farever_more_api::{ImageAsset, ImageRef};
use image::{ImageReader, Limits, RgbaImage};

const MAX_ICON_EDGE: u32 = 512;
const MAX_ATLAS_EDGE: u32 = 8_192;
const MAX_ENCODED_ATLAS_BYTES: u64 = 32 * 1024 * 1024;
const MAX_DECODED_ATLAS_BYTES: usize = 128 * 1024 * 1024;
const MAX_SKILL_IMAGES: usize = 1_024;
const MAX_SKILL_IMAGE_BYTES: usize = 64 * 1024 * 1024;

/// Process-local registry of immutable skill-icon crops.
pub struct SkillImages {
    pak: HeapsPak,
    atlases: HashMap<String, RgbaImage>,
    references: HashMap<IconCrop, ImageRef>,
    rejected: HashSet<IconCrop>,
    assets: Vec<ImageAsset>,
    atlas_bytes: usize,
    total_bytes: usize,
}

impl SkillImages {
    /// Opens the packaged full-resource archive without decoding image data.
    pub fn open(game: &GameInstall) -> Result<Self, String> {
        let pak = HeapsPak::open(game.directory.join("res.pak"))
            .map_err(|error| format!("open res.pak: {error}"))?;
        Ok(Self {
            pak,
            atlases: HashMap::new(),
            references: HashMap::new(),
            rejected: HashSet::new(),
            assets: Vec::new(),
            atlas_bytes: 0,
            total_bytes: 0,
        })
    }

    /// Resolves and lazily decodes one game-authored atlas crop.
    pub fn resolve(&mut self, icon: &IconCrop) -> Result<Option<ImageRef>, String> {
        if let Some(reference) = self.references.get(icon) {
            return Ok(Some(reference.clone()));
        }
        if self.rejected.contains(icon) {
            return Ok(None);
        }
        let result = self.resolve_new(icon);
        if result.is_err() {
            self.rejected.insert(icon.clone());
        }
        result.map(Some)
    }

    fn resolve_new(&mut self, icon: &IconCrop) -> Result<ImageRef, String> {
        if self.assets.len() >= MAX_SKILL_IMAGES {
            return Err(format!(
                "skill image count exceeds limit {MAX_SKILL_IMAGES}"
            ));
        }

        if !self.atlases.contains_key(icon.atlas_path) {
            let entry = self
                .pak
                .file(icon.atlas_path)
                .map_err(|error| format!("find atlas {:?}: {error}", icon.atlas_path))?;
            if entry.size > MAX_ENCODED_ATLAS_BYTES {
                return Err(format!(
                    "encoded atlas {:?} exceeds byte limit {MAX_ENCODED_ATLAS_BYTES}",
                    icon.atlas_path
                ));
            }
            let encoded = self
                .pak
                .read(icon.atlas_path)
                .map_err(|error| format!("read atlas {:?}: {error}", icon.atlas_path))?;
            let remaining_bytes = MAX_DECODED_ATLAS_BYTES.saturating_sub(self.atlas_bytes);
            let atlas = decode_atlas(&encoded, remaining_bytes)
                .map_err(|error| format!("decode atlas {:?}: {error}", icon.atlas_path))?;
            let atlas_bytes = atlas.as_raw().len();
            let total_atlas_bytes = self.atlas_bytes.saturating_add(atlas_bytes);
            if total_atlas_bytes > MAX_DECODED_ATLAS_BYTES {
                return Err(format!(
                    "decoded atlases exceed byte limit {MAX_DECODED_ATLAS_BYTES}"
                ));
            }
            self.atlases.insert(icon.atlas_path.to_owned(), atlas);
            self.atlas_bytes = total_atlas_bytes;
        }
        let crop = crop_icon(
            self.atlases
                .get(icon.atlas_path)
                .expect("atlas inserted immediately above"),
            icon,
        )?;
        let bytes = crop.into_raw();
        let total_bytes = self.total_bytes.saturating_add(bytes.len());
        if total_bytes > MAX_SKILL_IMAGE_BYTES {
            return Err(format!(
                "skill image bytes exceed limit {MAX_SKILL_IMAGE_BYTES}"
            ));
        }

        let reference = ImageRef {
            id: format!("game/skill-icon/{}", self.assets.len() + 1),
        };
        self.assets.push(ImageAsset {
            id: reference.id.clone(),
            width: icon.width,
            height: icon.height,
            rgba: Arc::from(bytes),
        });
        self.total_bytes = total_bytes;
        self.references.insert(icon.clone(), reference.clone());
        Ok(reference)
    }

    /// Returns a cheap clone of every image currently retained by the host.
    pub fn assets(&self) -> Vec<ImageAsset> {
        self.assets.clone()
    }

    /// Number of immutable image resources currently registered.
    #[must_use]
    pub fn len(&self) -> usize {
        self.assets.len()
    }
}

// The archive preserves .png paths even when Heaps compiles them to DDS.
// Select by payload signature, with the same limits for both encodings.
fn decode_atlas(encoded: &[u8], max_decoded_bytes: usize) -> Result<RgbaImage, String> {
    if encoded.starts_with(b"DDS ") {
        return farever_db::texture::decode_bc7_dds(encoded, MAX_ATLAS_EDGE, max_decoded_bytes);
    }
    let mut reader = ImageReader::new(Cursor::new(encoded));
    reader.set_format(image::ImageFormat::Png);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_ATLAS_EDGE);
    limits.max_image_height = Some(MAX_ATLAS_EDGE);
    limits.max_alloc = Some(max_decoded_bytes as u64);
    reader.limits(limits);
    let atlas = reader
        .decode()
        .map_err(|error| error.to_string())?
        .into_rgba8();
    if atlas.as_raw().len() > max_decoded_bytes {
        return Err(format!(
            "decoded atlas exceeds byte limit {max_decoded_bytes}"
        ));
    }
    Ok(atlas)
}

fn crop_icon(atlas: &RgbaImage, icon: &IconCrop) -> Result<RgbaImage, String> {
    if icon.width == 0
        || icon.height == 0
        || icon.width > MAX_ICON_EDGE
        || icon.height > MAX_ICON_EDGE
    {
        return Err(format!(
            "invalid icon dimensions {}x{} for {:?}",
            icon.width, icon.height, icon.atlas_path
        ));
    }
    let right = icon
        .x
        .checked_add(icon.width)
        .ok_or_else(|| "skill icon horizontal bounds overflow".to_owned())?;
    let bottom = icon
        .y
        .checked_add(icon.height)
        .ok_or_else(|| "skill icon vertical bounds overflow".to_owned())?;
    if right > atlas.width() || bottom > atlas.height() {
        return Err(format!(
            "icon crop ({}, {}) {}x{} exceeds atlas {:?} {}x{}",
            icon.x,
            icon.y,
            icon.width,
            icon.height,
            icon.atlas_path,
            atlas.width(),
            atlas.height()
        ));
    }
    Ok(image::imageops::crop_imm(atlas, icon.x, icon.y, icon.width, icon.height).to_image())
}

#[cfg(test)]
mod tests {
    use super::*;
    use farever_db::{discover_game, Inventory};

    #[test]
    fn retains_png_support_and_rejects_invalid_or_over_budget_data() {
        let atlas = RgbaImage::from_pixel(2, 1, image::Rgba([200, 40, 80, 123]));
        let mut encoded = Cursor::new(Vec::new());
        atlas
            .write_to(&mut encoded, image::ImageFormat::Png)
            .unwrap();
        assert_eq!(decode_atlas(encoded.get_ref(), 1024).unwrap(), atlas);
        assert!(decode_atlas(encoded.get_ref(), 7).is_err());
        assert!(decode_atlas(b"DDS ", 1024).is_err());
        assert!(decode_atlas(b"unsupported image", 1024).is_err());
    }

    #[test]
    fn crops_the_declared_pixel_rectangle() {
        let mut atlas = RgbaImage::new(4, 2);
        for y in 0..2 {
            for x in 0..4 {
                atlas.put_pixel(x, y, image::Rgba([x as u8, y as u8, 0, 255]));
            }
        }
        let crop = crop_icon(
            &atlas,
            &IconCrop {
                atlas_path: "fixture.png",
                x: 2,
                y: 0,
                width: 2,
                height: 2,
            },
        )
        .unwrap();

        assert_eq!(crop.dimensions(), (2, 2));
        assert_eq!(crop.get_pixel(0, 1).0, [2, 1, 0, 255]);
    }

    #[test]
    fn current_install_loads_all_skill_icons_when_available() {
        let Ok(game) = discover_game() else { return };
        let mut checked = 0;
        let paths: HashSet<_> = Inventory::skills()
            .iter()
            .filter_map(|skill| skill.icon.as_ref().map(|icon| icon.atlas_path))
            .collect();
        // Cover all classes without retaining the entire game's atlas collection
        // at once: the live registry deliberately caps its cumulative cache.
        for path in &paths {
            let mut images = SkillImages::open(&game).unwrap();
            let mut seen = HashSet::new();
            for skill in Inventory::skills().iter().filter(|skill| {
                skill
                    .icon
                    .as_ref()
                    .is_some_and(|icon| icon.atlas_path == *path)
            }) {
                let Some(icon) = skill.icon.as_ref() else {
                    continue;
                };
                if !seen.insert(icon) {
                    continue;
                }
                let reference = images
                    .resolve(icon)
                    .unwrap_or_else(|error| panic!("skill {}: {error}", skill.id))
                    .unwrap();
                let asset = images
                    .assets
                    .iter()
                    .find(|asset| asset.id == reference.id)
                    .unwrap();

                assert_eq!((asset.width, asset.height), (icon.width, icon.height));
                assert_eq!(asset.rgba.len(), (icon.width * icon.height * 4) as usize);
                if ["GS_Nova_Combo", "Axe_Base_Attack"].contains(&skill.id) {
                    assert!(asset.rgba.chunks_exact(4).any(|pixel| pixel[3] != 0));
                }
                assert_eq!(images.resolve(icon).unwrap(), Some(reference));
                checked += 1;
            }
        }
        assert!(checked > 0);
        eprintln!(
            "Loaded {checked} unique skill crops from {} installed atlases",
            paths.len()
        );
    }
}
