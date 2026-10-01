//! Offline verification of the curated sites against their installed map tiles.
use crate::{pak::HeapsPak, GameInstall, Inventory};
use serde_json::{Map, Value};
use std::io::{Cursor, Read};

/// Verify all eight reviewed roots, including their item cost, demon, and position.
///
/// # Errors
/// Returns an error on a malformed tile or any changed summoning definition.
pub fn verify_map(game: &GameInstall) -> Result<(), String> {
    let pak = HeapsPak::open(game.directory.join("res.map.pak")).map_err(|e| e.to_string())?;
    for site in Inventory::soulstones() {
        let path = format!("Level/{}.dat/gameplayData/{}.prefab", site.world, site.tile);
        let data = pak.read(&path).map_err(|e| e.to_string())?;
        let value = Hbson::decode(&data).map_err(|e| format!("{path}: {e}"))?;
        let matches: Vec<_> = value
            .get("children")
            .and_then(Value::as_array)
            .ok_or("tile has no children")?
            .iter()
            .filter(|node| node.pointer("/props/id").and_then(Value::as_str) == Some(site.site))
            .collect();
        if matches.len() != 1 {
            return Err(format!("{path}: expected one {}", site.site));
        }
        let node = matches[0];
        let cost = node
            .pointer("/props/props/interactible/cost")
            .and_then(Value::as_array);
        if cost.is_none_or(|cost| {
            cost.len() != 1
                || cost[0].get("item").and_then(Value::as_str) != Some(site.item)
                || cost[0].get("count").and_then(Value::as_u64) != Some(1)
        }) || node
            .pointer("/props/props/spawnUnit/unit")
            .and_then(Value::as_str)
            != Some(site.demon)
            || node.get("type").and_then(Value::as_str) != Some("object")
        {
            return Err(format!(
                "{path}: summoning definition changed for {}",
                site.site
            ));
        }
        for (axis, expected) in [("x", site.x), ("y", site.y), ("z", site.z)] {
            if node.get(axis).and_then(Value::as_f64).map(|v| v as f32) != Some(expected) {
                return Err(format!("{path}: {} {axis} changed", site.site));
            }
        }
    }
    Ok(())
}

// Heaps HBSON reader: only the documented scalar/object/array format is accepted.
// https://github.com/HeapsIO/heaps/blob/master/hxd/fmt/hbson/Reader.hx
struct Hbson<'a> {
    input: Cursor<&'a [u8]>,
    strings: Vec<String>,
}
impl<'a> Hbson<'a> {
    fn decode(data: &'a [u8]) -> Result<Value, String> {
        if data.len() > 4 * 1024 * 1024 || !data.starts_with(b"HBSON\0") {
            return Err("invalid HBSON tile".into());
        }
        let mut reader = Self {
            input: Cursor::new(&data[6..]),
            strings: Vec::new(),
        };
        let value = reader.value(0)?;
        if reader.input.position() as usize != data.len() - 6 {
            return Err("trailing tile bytes".into());
        }
        Ok(value)
    }
    fn bytes<const N: usize>(&mut self) -> Result<[u8; N], String> {
        let mut bytes = [0; N];
        self.input
            .read_exact(&mut bytes)
            .map_err(|_| "truncated tile")?;
        Ok(bytes)
    }
    fn string(&mut self) -> Result<String, String> {
        let index = u32::from_le_bytes(self.bytes()?);
        if index & 0xc0000000 == 0 {
            return self
                .strings
                .get(index as usize)
                .cloned()
                .ok_or("invalid string reference".into());
        }
        let len = (index & 0x3fffffff) as usize;
        if len > 1024 * 1024 {
            return Err("oversized tile string".into());
        }
        let mut bytes = vec![0; len];
        self.input
            .read_exact(&mut bytes)
            .map_err(|_| "truncated string")?;
        let value = String::from_utf8(bytes).map_err(|_| "invalid UTF-8")?;
        if index & 0x40000000 != 0 {
            self.strings.push(value.clone());
        }
        Ok(value)
    }
    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > 64 {
            return Err("tile nesting too deep".into());
        }
        let code = self.bytes::<1>()?[0];
        Ok(match code {
            0 => Value::from(0),
            1 => Value::from(self.bytes::<1>()?[0]),
            2 => Value::from(i32::from_le_bytes(self.bytes()?)),
            3 => {
                let value = f64::from_le_bytes(self.bytes()?);
                if !value.is_finite() {
                    return Err("nonfinite tile value".into());
                }
                Value::from(value)
            }
            4 => Value::Bool(true),
            5 => Value::Bool(false),
            6 => Value::Null,
            7 => Value::Object(Map::new()),
            8 | 9 | 12 | 13 => {
                let count = if code == 8 || code == 12 {
                    self.bytes::<1>()?[0] as usize
                } else {
                    u32::from_le_bytes(self.bytes()?) as usize
                };
                if count > 100_000 {
                    return Err("oversized tile collection".into());
                }
                if code == 8 || code == 9 {
                    let mut fields = Map::new();
                    for _ in 0..count {
                        let name = self.string()?;
                        let value = self.value(depth + 1)?;
                        if fields.insert(name, value).is_some() {
                            return Err("duplicate tile field".into());
                        }
                    }
                    Value::Object(fields)
                } else {
                    let mut values = Vec::new();
                    for _ in 0..count {
                        values.push(self.value(depth + 1)?);
                    }
                    Value::Array(values)
                }
            }
            10 => Value::String(self.string()?),
            11 => Value::Array(Vec::new()),
            _ => return Err(format!("unsupported HBSON tag {code}")),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_named_soulstone_has_one_valid_site() {
        let items: Vec<_> = Inventory::items()
            .iter()
            .filter(|item| item.item_type == Some("Soulstone"))
            .collect();
        assert_eq!(items.len(), 8);
        assert_eq!(Inventory::soulstones().len(), items.len());
        for item in items {
            let sites: Vec<_> = Inventory::soulstones()
                .iter()
                .filter(|site| site.item == item.id)
                .collect();
            assert_eq!(sites.len(), 1);
            let site = sites[0];
            assert_eq!(site.world, "World/W1_Siagarta");
            assert!(Inventory::unit(site.demon)
                .and_then(|unit| unit.name)
                .is_some());
            assert!([site.x, site.y, site.z].iter().all(|v| v.is_finite()));
        }
    }
    #[test]
    fn malformed_tiles_are_rejected() {
        for bytes in [
            b"HBSON".as_slice(),
            b"HBSON\0\xff",
            b"HBSON\0\x09\xff\xff\xff\x7f",
            b"HBSON\0\x03\0",
            b"HBSON\0\x07\0",
        ] {
            assert!(Hbson::decode(bytes).is_err());
        }
    }
}
