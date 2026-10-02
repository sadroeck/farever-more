//! Bounded decoding of the compiled DDS textures stored under game PNG paths.

use image::RgbaImage;

/// Decode the top mip of a single 2D `BC7_UNORM` DDS texture into straight RGBA.
///
/// The caller bounds encoded input before reading it. Dimensions and decoded
/// bytes are checked here before allocation; trailing lower mips are ignored.
/// Header layout: <https://learn.microsoft.com/en-us/windows/win32/direct3ddds/dds-header-dxt10>.
///
/// # Errors
///
/// Rejects truncated data, unsupported formats/surfaces/alpha modes, and images
/// exceeding either caller-supplied limit.
pub fn decode_bc7_dds(
    encoded: &[u8],
    max_edge: u32,
    max_decoded_bytes: usize,
) -> Result<RgbaImage, String> {
    const HEADER_BYTES: usize = 148;
    if encoded.len() < HEADER_BYTES || &encoded[..4] != b"DDS " {
        return Err("expected complete DDS header".into());
    }
    let word = |offset| {
        u32::from_le_bytes(
            encoded[offset..offset + 4]
                .try_into()
                .expect("header checked"),
        )
    };
    if word(4) != 124 || word(76) != 32 || word(80) & 4 == 0 || &encoded[84..88] != b"DX10" {
        return Err("expected DDS DX10 pixel format".into());
    }
    // Only the reviewed 2D BC7_UNORM format. Arrays, cube maps, volume textures,
    // sRGB and premultiplied alpha need their own verified conversion policy.
    if word(128) != 98
        || word(132) != 3
        || word(136) != 0
        || word(140) != 1
        || word(24) > 1
        || word(112) != 0
        || word(144) > 1
    {
        return Err("expected single 2D BC7_UNORM texture with straight alpha".into());
    }
    let (width, height) = (word(16), word(12));
    if width == 0 || height == 0 || width > max_edge || height > max_edge {
        return Err(format!(
            "DDS dimensions {width}x{height} exceed edge limit {max_edge}"
        ));
    }
    let pixel_count = u64::from(width) * u64::from(height);
    let decoded_bytes = pixel_count
        .checked_mul(4)
        .and_then(|bytes| usize::try_from(bytes).ok())
        .filter(|bytes| *bytes <= max_decoded_bytes)
        .ok_or_else(|| format!("decoded DDS exceeds byte limit {max_decoded_bytes}"))?;
    // BC7 stores 16 bytes per 4x4 block, including partial edge blocks.
    let block_bytes = u64::from(width.div_ceil(4)) * u64::from(height.div_ceil(4)) * 16;
    let data_end = usize::try_from(block_bytes)
        .ok()
        .and_then(|bytes| HEADER_BYTES.checked_add(bytes))
        .ok_or("DDS top mip extent overflow")?;
    let blocks = encoded
        .get(HEADER_BYTES..data_end)
        .ok_or("truncated DDS top mip")?;
    let mut pixels = vec![0; decoded_bytes / 4];
    texture2ddecoder::decode_bc7(blocks, width as usize, height as usize, &mut pixels)
        .map_err(|error| format!("decode DDS BC7: {error}"))?;
    // texture2ddecoder returns packed 0xAARRGGBB words, not RGBA byte order.
    let rgba = pixels
        .into_iter()
        .flat_map(|pixel| {
            let [b, g, r, a] = pixel.to_le_bytes();
            [r, g, b, a]
        })
        .collect();
    RgbaImage::from_raw(width, height, rgba).ok_or_else(|| "invalid decoded DDS pixels".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid_dds(width: u32, height: u32) -> Vec<u8> {
        let mut encoded = vec![0; 148];
        encoded[..4].copy_from_slice(b"DDS ");
        encoded[84..88].copy_from_slice(b"DX10");
        for (offset, value) in [
            (4, 124u32),
            (12, height),
            (16, width),
            (76, 32),
            (80, 4),
            (128, 98),
            (132, 3),
            (140, 1),
        ] {
            encoded[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        // BC7 mode 6: equal RGBA endpoints (200,40,80,254), zero P-bits and
        // indices. Distinct red/blue and nonopaque alpha catch channel mistakes.
        let mut block = 1u128 << 6;
        let mut shift = 7;
        for endpoint in [100u128, 100, 20, 20, 40, 40, 127, 127] {
            block |= endpoint << shift;
            shift += 7;
        }
        for _ in 0..width.div_ceil(4) * height.div_ceil(4) {
            encoded.extend_from_slice(&block.to_le_bytes());
        }
        encoded
    }

    #[test]
    fn decodes_top_mip_with_rgba_channels_alpha_and_partial_blocks() {
        let mut encoded = solid_dds(5, 3);
        encoded[28..32].copy_from_slice(&3u32.to_le_bytes());
        encoded.extend_from_slice(&[0xFF; 32]); // Lower mips must not affect top mip.
        let atlas = decode_bc7_dds(&encoded, 512, 1024).unwrap();
        assert_eq!(atlas.dimensions(), (5, 3));
        assert!(atlas.pixels().all(|pixel| pixel.0 == [200, 40, 80, 254]));
    }

    #[test]
    fn rejects_truncation_unsupported_surfaces_and_allocation_limits() {
        let valid = solid_dds(4, 4);
        for length in [0, 4, 127, 147, valid.len() - 1] {
            assert!(decode_bc7_dds(&valid[..length], 512, 1024).is_err());
        }
        for (offset, value) in [
            (4, 123u32),
            (76, 31),
            (80, 0),
            (128, 99),
            (132, 4),
            (136, 4),
            (140, 2),
            (24, 2),
            (112, 0x200),
            (144, 2),
            (12, 0),
            (16, 8193),
            (12, u32::MAX),
        ] {
            let mut changed = valid.clone();
            changed[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(
                decode_bc7_dds(&changed, 8192, 128 * 1024 * 1024).is_err(),
                "offset={offset}"
            );
        }
        assert!(decode_bc7_dds(&valid, 3, 1024).is_err());
        assert!(decode_bc7_dds(&valid, 512, 63).is_err());
        let mut changed = valid;
        changed[84..88].copy_from_slice(b"DXT5");
        assert!(decode_bc7_dds(&changed, 512, 1024).is_err());
    }
}
