//! Read-only snapshot of the GPS-owned destination; bounded, versioned wire format.
use farever_more_sdk::dependencies::{Dependencies, Service};

pub const SERVICE_ID: &str = "waypoint";
pub const CURRENT_OPERATION: u32 = 1;
const MAX_TEXT: usize = 512;

#[derive(Clone, Debug, PartialEq)]
pub struct Waypoint {
    pub process_session: u64,
    pub world: String,
    pub position: [f32; 3],
    pub name: String,
    /// Inventory item whose portrait identifies this destination, when present.
    pub item_id: Option<String>,
}

pub struct Client(Service);
impl Client {
    pub fn open(dependencies: &Dependencies) -> Result<Self, String> {
        dependencies
            .open("gps", SERVICE_ID)
            .map(Self)
            .map_err(|e| e.to_string())
    }
    pub fn current(&self) -> Result<Option<Waypoint>, String> {
        decode(
            &self
                .0
                .call(CURRENT_OPERATION, &[])
                .map_err(|e| e.to_string())?,
        )
    }
}

pub fn encode(waypoint: Option<&Waypoint>) -> Result<Vec<u8>, String> {
    let mut bytes = vec![2, u8::from(waypoint.is_some())];
    if let Some(point) = waypoint {
        if !point.position.iter().all(|v| v.is_finite()) {
            return Err("nonfinite waypoint".into());
        }
        bytes.extend(point.process_session.to_le_bytes());
        for value in point.position {
            bytes.extend(value.to_le_bytes());
        }
        for value in [
            point.world.as_str(),
            point.name.as_str(),
            point.item_id.as_deref().unwrap_or(""),
        ] {
            if value.len() > MAX_TEXT {
                return Err("waypoint text too long".into());
            }
            bytes.extend((value.len() as u16).to_le_bytes());
            bytes.extend(value.as_bytes());
        }
    }
    Ok(bytes)
}

pub fn decode(bytes: &[u8]) -> Result<Option<Waypoint>, String> {
    if bytes == [1, 0] || bytes == [2, 0] {
        return Ok(None);
    }
    if bytes.len() < 26 || !matches!(bytes[..2], [1, 1] | [2, 1]) {
        return Err("invalid waypoint header".into());
    }
    let process_session = u64::from_le_bytes(bytes[2..10].try_into().unwrap());
    let position = std::array::from_fn(|i| {
        f32::from_le_bytes(bytes[10 + i * 4..14 + i * 4].try_into().unwrap())
    });
    if !position.iter().all(|v| v.is_finite()) {
        return Err("nonfinite waypoint".into());
    }
    let mut at = 22;
    let mut text = || -> Result<String, String> {
        let len = bytes.get(at..at + 2).ok_or("truncated waypoint text")?;
        let len = u16::from_le_bytes(len.try_into().unwrap()) as usize;
        at += 2;
        if len > MAX_TEXT {
            return Err("waypoint text too long".into());
        }
        let value = std::str::from_utf8(bytes.get(at..at + len).ok_or("truncated waypoint text")?)
            .map_err(|_| "invalid waypoint UTF-8")?
            .to_owned();
        at += len;
        Ok(value)
    };
    let world = text()?;
    let name = text()?;
    let item_id = if bytes[0] == 2 {
        let item = text()?;
        (!item.is_empty()).then_some(item)
    } else {
        None
    };
    if at != bytes.len() {
        return Err("trailing waypoint bytes".into());
    }
    Ok(Some(Waypoint {
        process_session,
        world,
        position,
        name,
        item_id,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_truncation_nonfinite_and_unknown_versions() {
        let point = Waypoint {
            process_session: 7,
            world: "World/W1_Siagarta".into(),
            position: [1., 2., 3.],
            name: "Asmodeaf".into(),
            item_id: Some("Soulstone_Z2_2".into()),
        };
        let encoded = encode(Some(&point)).unwrap();
        assert_eq!(decode(&encoded).unwrap(), Some(point.clone()));
        for end in 0..encoded.len() {
            assert!(decode(&encoded[..end]).is_err());
        }
        let mut bad = encoded.clone();
        bad[0] = 3;
        assert!(decode(&bad).is_err());
        let mut bad = encoded.clone();
        bad[10..14].copy_from_slice(&f32::NAN.to_le_bytes());
        assert!(decode(&bad).is_err());
        let mut bad = encoded;
        bad.push(0);
        assert!(decode(&bad).is_err());
        assert_eq!(decode(&encode(None).unwrap()).unwrap(), None);
        // Version 1 remains readable, with the generic destination marker.
        let mut old = encode(Some(&point)).unwrap();
        old[0] = 1;
        old.truncate(old.len() - 2 - point.item_id.as_ref().unwrap().len());
        let mut expected = point;
        expected.item_id = None;
        assert_eq!(decode(&old).unwrap(), Some(expected.clone()));
        assert_eq!(
            decode(&encode(Some(&expected)).unwrap()).unwrap(),
            Some(expected)
        );
        assert_eq!(decode(&[1, 0]).unwrap(), None);
    }
}
