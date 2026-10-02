//! Project reviewed soulstone sites from the inventory into the sandboxed GPS.
use farever_db::Inventory;
use std::{env, fmt::Write, fs, path::Path};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let path = args
        .next()
        .ok_or("usage: soulstones <output.rs> [--check]")?;
    if path == "--icons" {
        let directory = args.next().ok_or("expected icon output directory")?;
        let check = match args.next().as_deref() {
            None => false,
            Some("--check") => true,
            _ => return Err("expected --check".into()),
        };
        if args.next().is_some() {
            return Err("unexpected argument".into());
        }
        return export_icons(Path::new(&directory), check);
    }
    if path == "--verify-map" {
        let game = if let Some(directory) = args.next() {
            farever_db::GameInstall::open(directory)?
        } else {
            farever_db::discover_game()?
        };
        farever_db::soulstones::verify_map(&game)?;
        println!(
            "Verified all {} W1 soulstone sites",
            Inventory::soulstones().len()
        );
        return Ok(());
    }
    let check = match args.next().as_deref() {
        None => false,
        Some("--check") => true,
        _ => return Err("expected --check".into()),
    };
    if args.next().is_some() {
        return Err("unexpected argument".into());
    }
    let mut out = String::from("// Generated from farever_db::Inventory::soulstones(). Do not edit.\nuse super::Soulstone;\n\npub const SITES: &[Soulstone] = &[\n");
    for site in Inventory::soulstones() {
        let item = Inventory::item(site.item).ok_or("soulstone item missing")?;
        let name = Inventory::unit(site.demon)
            .and_then(|unit| unit.name)
            .ok_or("demon name missing")?;
        if item.item_type != Some("Soulstone")
            || ![site.x, site.y, site.z].iter().all(|v| v.is_finite())
        {
            return Err("invalid soulstone site".into());
        }
        writeln!(out, "    Soulstone {{ item: {:?}, name: {:?}, world: {:?}, position: [{:?}, {:?}, {:?}] }},", site.item, name, site.world, site.x, site.y, site.z)?;
    }
    out.push_str("];\n");
    if check {
        if fs::read_to_string(&path)?.replace("\r\n", "\n") != out {
            return Err("GPS soulstone table is stale".into());
        }
    } else {
        fs::write(path, out)?;
    }
    Ok(())
}

// Import only the reviewed full portraits, checking item.gfx (a whitelisted
// sheet/column) without depending on unrelated CastleDB extraction tables.
#[cfg(not(feature = "portrait-import"))]
fn export_icons(_directory: &Path, _check: bool) -> Result<(), Box<dyn std::error::Error>> {
    Err("--icons requires --features portrait-import".into())
}

#[cfg(feature = "portrait-import")]
fn export_icons(directory: &Path, check: bool) -> Result<(), Box<dyn std::error::Error>> {
    let game = farever_db::discover_game()?;
    let document = game.load_cdb()?;
    let item_sheets: Vec<_> = document["sheets"]
        .as_array()
        .ok_or("missing sheets")?
        .iter()
        .filter(|sheet| sheet["name"] == "item")
        .collect();
    if item_sheets.len() != 1 {
        return Err("expected one item sheet".into());
    }
    let items = item_sheets[0]["lines"]
        .as_array()
        .ok_or("missing item rows")?;
    let pak = farever_db::pak::HeapsPak::open(game.directory.join("res.pak"))?;
    if !check {
        fs::create_dir_all(directory)?;
    }
    let mut generated = String::from("// Generated from reviewed Inventory::soulstones() portraits. Do not edit.\n\npub const SOURCES: &[(&str, &[u8])] = &[\n");
    for site in Inventory::soulstones() {
        let matches: Vec<_> = items.iter().filter(|row| row["id"] == site.item).collect();
        if matches.len() != 1
            || matches[0]["gfx"]
                != serde_json::json!({
                    "file": site.icon_path, "size": 256, "x": 0, "y": 0
                })
        {
            return Err(format!("{} inventory portrait definition changed", site.item).into());
        }
        if pak.file(site.icon_path)?.size > 1024 * 1024 {
            return Err("oversized portrait".into());
        }
        let png = portrait_png(&pak.read(site.icon_path)?)?;
        let filename = format!("{}.png", site.item);
        let output = directory.join(&filename);
        if check {
            if fs::read(&output)? != png {
                return Err(format!("{} portrait is stale", site.item).into());
            }
        } else {
            fs::write(&output, png)?;
        }
        writeln!(
            generated,
            "    ({:?}, include_bytes!({filename:?})),",
            site.item
        )?;
    }
    generated.push_str("];\n");
    let output = directory.join("icons_generated.rs");
    if check {
        if fs::read_to_string(output)?.replace("\r\n", "\n") != generated {
            return Err("portrait mapping is stale".into());
        }
    } else {
        fs::write(output, generated)?;
    }
    println!(
        "{} all {} soulstone inventory portraits",
        if check { "Verified" } else { "Imported" },
        Inventory::soulstones().len()
    );
    Ok(())
}

// Heaps compiles these .png entries to DDS DX10 / BC7. Decode only the reviewed
// full-size 2D portrait's top mip; no recoloring, resampling, or custom artwork.
#[cfg(feature = "portrait-import")]
fn portrait_png(dds: &[u8]) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    const EDGE: usize = 256;
    const DATA_BYTES: usize = EDGE * EDGE; // BC7: 16 bytes per 4x4 block.
    if dds.len() < 148 + DATA_BYTES || &dds[..4] != b"DDS " || &dds[84..88] != b"DX10" {
        return Err("expected complete DDS DX10 soulstone portrait".into());
    }
    let word = |offset| u32::from_le_bytes(dds[offset..offset + 4].try_into().unwrap());
    if word(4) != 124
        || word(12) != 256
        || word(16) != 256
        || word(76) != 32
        || word(128) != 98
        || word(132) != 3
        || word(136) != 0
        || word(140) != 1
    {
        return Err("expected 256x256 2D BC7_UNORM portrait".into());
    }
    let portrait = farever_db::texture::decode_bc7_dds(dds, 256, EDGE * EDGE * 4)?;
    let mut output = std::io::Cursor::new(Vec::new());
    portrait.write_to(&mut output, image::ImageFormat::Png)?;
    Ok(output.into_inner())
}

#[cfg(all(test, feature = "portrait-import"))]
mod tests {
    use super::*;
    #[test]
    fn portrait_decoder_rejects_truncated_and_changed_dds_formats() {
        for input in [vec![], vec![0; 148], vec![0; 148 + 65536]] {
            assert!(portrait_png(&input).is_err());
        }
        let mut header = vec![0; 148 + 65536];
        header[..4].copy_from_slice(b"DDS ");
        header[84..88].copy_from_slice(b"DX10");
        for (offset, word) in [
            (4, 124u32),
            (12, 256),
            (16, 256),
            (76, 32),
            (80, 4),
            (128, 98),
            (132, 3),
            (140, 1),
        ] {
            header[offset..offset + 4].copy_from_slice(&word.to_le_bytes());
        }
        assert!(portrait_png(&header[..148 + 65535]).is_err());
        for (offset, value) in [(12, 512u32), (128, 99), (140, 2)] {
            let mut changed = header.clone();
            changed[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(portrait_png(&changed).is_err());
        }
    }
}
