//! Bounded PBF wire decoder written against the public OSM-binary schema.
//! Supports raw/zlib blocks and ordinary/dense nodes; no copied protobuf code.
use flate2::read::ZlibDecoder;
use routeweft_dataset::{invalid, Error, Point, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
pub type Tags = BTreeMap<String, String>;
pub struct Node {
    pub id: i64,
    pub point: Point,
    pub tags: Tags,
}
pub struct Way {
    pub id: i64,
    pub refs: Vec<i64>,
    pub tags: Tags,
}
pub struct Member {
    pub id: i64,
    pub kind: u64,
    pub role: String,
}
pub struct Relation {
    pub id: i64,
    pub members: Vec<Member>,
    pub tags: Tags,
}
#[derive(Default)]
pub struct Osm {
    pub nodes: BTreeMap<i64, Node>,
    pub ways: BTreeMap<i64, Way>,
    pub relations: Vec<Relation>,
    pub snapshot: Option<i64>,
    estimated_bytes: usize,
    relation_ids: BTreeSet<i64>,
}
impl Osm {
    fn charge(&mut self, bytes: usize) -> Result<()> {
        self.estimated_bytes = self
            .estimated_bytes
            .checked_add(bytes)
            .ok_or(Error::LimitExceeded)?;
        if self.estimated_bytes > 512 * 1024 * 1024 {
            return Err(Error::LimitExceeded);
        }
        Ok(())
    }
}
fn tag_bytes(tags: &Tags) -> usize {
    tags.iter().map(|(k, v)| k.len() + v.len() + 96).sum()
}
const BLOCK: usize = 32 * 1024 * 1024;
const OBJECTS: usize = 2_000_000;
#[derive(Clone, Copy)]
enum Value<'a> {
    Int(u64),
    Bytes(&'a [u8]),
}
fn var(bytes: &[u8], pos: &mut usize) -> Result<u64> {
    let mut v = 0;
    for shift in (0..70).step_by(7) {
        let b = *bytes.get(*pos).ok_or_else(|| invalid("truncated varint"))?;
        *pos += 1;
        if shift == 63 && b > 1 {
            return Err(invalid("varint overflow"));
        }
        v |= u64::from(b & 127) << shift;
        if b & 128 == 0 {
            return Ok(v);
        }
    }
    Err(invalid("varint overflow"))
}
fn fields(bytes: &[u8]) -> Result<Vec<(u64, Value<'_>)>> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < bytes.len() {
        if out.len() >= 1_000_000 {
            return Err(Error::LimitExceeded);
        }
        let tag = var(bytes, &mut p)?;
        if tag >> 3 == 0 {
            return Err(invalid("field zero"));
        }
        let value = match tag & 7 {
            0 => Value::Int(var(bytes, &mut p)?),
            2 => {
                let len = usize::try_from(var(bytes, &mut p)?).map_err(|_| Error::LimitExceeded)?;
                let end = p
                    .checked_add(len)
                    .filter(|&v| v <= bytes.len())
                    .ok_or_else(|| invalid("field length"))?;
                let v = Value::Bytes(&bytes[p..end]);
                p = end;
                v
            }
            1 | 5 => {
                let n = if tag & 7 == 1 { 8 } else { 4 };
                p = p
                    .checked_add(n)
                    .filter(|&v| v <= bytes.len())
                    .ok_or_else(|| invalid("fixed field length"))?;
                continue;
            }
            _ => return Err(invalid("unsupported protobuf wire type")),
        };
        out.push((tag >> 3, value));
    }
    Ok(out)
}
fn int(f: &[(u64, Value<'_>)], id: u64, default: Option<u64>) -> Result<u64> {
    match f.iter().find(|(k, _)| *k == id).map(|v| v.1) {
        Some(Value::Int(v)) => Ok(v),
        None => default.ok_or_else(|| invalid("required integer")),
        _ => Err(invalid("integer wire type")),
    }
}
fn blob<'a>(f: &[(u64, Value<'a>)], id: u64) -> Result<&'a [u8]> {
    match f.iter().find(|(k, _)| *k == id).map(|v| v.1) {
        Some(Value::Bytes(v)) => Ok(v),
        _ => Err(invalid("required byte field")),
    }
}
fn packed(f: &[(u64, Value<'_>)], id: u64) -> Result<Vec<u64>> {
    let mut out = Vec::new();
    for (_, value) in f.iter().filter(|(k, _)| *k == id) {
        match value {
            Value::Int(v) => out.push(*v),
            Value::Bytes(bytes) => {
                let mut p = 0;
                while p < bytes.len() {
                    out.push(var(bytes, &mut p)?);
                    if out.len() > OBJECTS {
                        return Err(Error::LimitExceeded);
                    }
                }
            }
        }
    }
    Ok(out)
}
fn signed(v: u64) -> i64 {
    ((v >> 1) as i64) ^ (-((v & 1) as i64))
}
fn delta(values: Vec<u64>) -> Result<Vec<i64>> {
    let mut n = 0_i64;
    values
        .into_iter()
        .map(|v| {
            n = n
                .checked_add(signed(v))
                .ok_or_else(|| invalid("delta overflow"))?;
            Ok(n)
        })
        .collect()
}
fn text(bytes: &[u8]) -> Result<String> {
    if bytes.len() > 65536 {
        return Err(Error::LimitExceeded);
    }
    String::from_utf8(bytes.to_vec()).map_err(|_| invalid("utf8"))
}
fn string(table: &[String], id: u64) -> Result<String> {
    table
        .get(usize::try_from(id).map_err(|_| Error::LimitExceeded)?)
        .cloned()
        .ok_or_else(|| invalid("string reference"))
}
fn tags(f: &[(u64, Value<'_>)], table: &[String]) -> Result<Tags> {
    let keys = packed(f, 2)?;
    let vals = packed(f, 3)?;
    if keys.len() != vals.len() || keys.len() > 4096 {
        return Err(invalid("tag arrays"));
    }
    keys.into_iter()
        .zip(vals)
        .map(|(k, v)| Ok((string(table, k)?, string(table, v)?)))
        .collect()
}
fn point(lon: i64, lat: i64, g: i64, lo: i64, la: i64) -> Result<Point> {
    let scale = |n: i64, o: i64| {
        n.checked_mul(g)
            .and_then(|v| v.checked_add(o))
            .map(|v| v as f64 * 1e-9)
            .ok_or_else(|| invalid("coordinate overflow"))
    };
    let p = Point {
        lon: scale(lon, lo)?,
        lat: scale(lat, la)?,
    };
    p.coordinate()?;
    Ok(p)
}
fn insert_node(osm: &mut Osm, node: Node) -> Result<()> {
    osm.charge(128 + tag_bytes(&node.tags))?;
    if osm.nodes.len() + osm.ways.len() + osm.relations.len() >= OBJECTS {
        return Err(Error::LimitExceeded);
    }
    if osm.nodes.insert(node.id, node).is_some() {
        return Err(invalid("duplicate node"));
    }
    Ok(())
}
fn primitive(bytes: &[u8], osm: &mut Osm) -> Result<()> {
    let f = fields(bytes)?;
    let table = fields(blob(&f, 1)?)?
        .into_iter()
        .filter(|(id, _)| *id == 1)
        .map(|(_, v)| match v {
            Value::Bytes(b) => text(b),
            _ => Err(invalid("string table")),
        })
        .collect::<Result<Vec<_>>>()?;
    let gran = int(&f, 17, Some(100))? as i64;
    if gran <= 0 {
        return Err(invalid("granularity"));
    }
    let lat_offset = int(&f, 19, Some(0))? as i64;
    let lon_offset = int(&f, 20, Some(0))? as i64;
    for (_, group) in f.iter().filter(|(k, _)| *k == 2) {
        let Value::Bytes(group) = group else {
            return Err(invalid("group"));
        };
        for (kind, item) in fields(group)? {
            let Value::Bytes(item) = item else {
                return Err(invalid("primitive wire type"));
            };
            let v = fields(item)?;
            match kind {
                1 => {
                    let node = Node {
                        id: signed(int(&v, 1, None)?),
                        point: point(
                            signed(int(&v, 9, None)?),
                            signed(int(&v, 8, None)?),
                            gran,
                            lon_offset,
                            lat_offset,
                        )?,
                        tags: tags(&v, &table)?,
                    };
                    insert_node(osm, node)?;
                }
                2 => {
                    let ids = delta(packed(&v, 1)?)?;
                    let lats = delta(packed(&v, 8)?)?;
                    let lons = delta(packed(&v, 9)?)?;
                    let kv = packed(&v, 10)?;
                    let mut pos = 0;
                    if ids.len() != lats.len() || ids.len() != lons.len() {
                        return Err(invalid("dense arrays"));
                    }
                    for ((id, lat), lon) in ids.into_iter().zip(lats).zip(lons) {
                        let mut tags = Tags::new();
                        if !kv.is_empty() {
                            loop {
                                let k = *kv.get(pos).ok_or_else(|| invalid("dense tags"))?;
                                pos += 1;
                                if k == 0 {
                                    break;
                                }
                                let val = *kv.get(pos).ok_or_else(|| invalid("dense tags"))?;
                                pos += 1;
                                tags.insert(string(&table, k)?, string(&table, val)?);
                                if tags.len() > 4096 {
                                    return Err(Error::LimitExceeded);
                                }
                            }
                        }
                        insert_node(
                            osm,
                            Node {
                                id,
                                point: point(lon, lat, gran, lon_offset, lat_offset)?,
                                tags,
                            },
                        )?;
                    }
                    if pos != kv.len() {
                        return Err(invalid("extra dense tags"));
                    }
                }
                3 => {
                    let way = Way {
                        id: int(&v, 1, None)? as i64,
                        refs: delta(packed(&v, 8)?)?,
                        tags: tags(&v, &table)?,
                    };
                    osm.charge(128 + way.refs.len() * 8 + tag_bytes(&way.tags))?;
                    if osm.ways.insert(way.id, way).is_some() {
                        return Err(invalid("duplicate way"));
                    }
                }
                4 => {
                    let relation_id = int(&v, 1, None)? as i64;
                    if !osm.relation_ids.insert(relation_id) {
                        return Err(invalid(format!("duplicate relation {relation_id}")));
                    }
                    let ids = delta(packed(&v, 9)?)?;
                    let roles = packed(&v, 8)?;
                    let kinds = packed(&v, 10)?;
                    if ids.len() != roles.len() || ids.len() != kinds.len() {
                        return Err(invalid(format!(
                            "relation {relation_id}: inconsistent member arrays"
                        )));
                    }
                    if kinds.iter().any(|k| *k > 2) {
                        return Err(invalid(format!(
                            "relation {relation_id}: invalid member type"
                        )));
                    }
                    let members = ids
                        .into_iter()
                        .zip(roles)
                        .zip(kinds)
                        .map(|((id, role), kind)| {
                            Ok(Member {
                                id,
                                kind,
                                role: string(&table, role)?,
                            })
                        })
                        .collect::<Result<_>>()?;
                    let members: Vec<Member> = members;
                    let tags = tags(&v, &table)?;
                    osm.charge(
                        128 + tag_bytes(&tags)
                            + members.iter().map(|m| 64 + m.role.len()).sum::<usize>(),
                    )?;
                    osm.relations.push(Relation {
                        id: relation_id,
                        members,
                        tags,
                    });
                }
                _ => {}
            }
            if osm.nodes.len() + osm.ways.len() + osm.relations.len() > OBJECTS {
                return Err(Error::LimitExceeded);
            }
        }
    }
    Ok(())
}
pub fn read(path: &Path) -> Result<Osm> {
    let mut input = std::fs::File::open(path)?;
    if input.metadata()?.len() > 512 * 1024 * 1024 {
        return Err(Error::LimitExceeded);
    }
    let mut osm = Osm::default();
    let mut header = false;
    let mut decoded = 0_usize;
    loop {
        let mut size = [0; 4];
        let n = input.read(&mut size[..1])?;
        if n == 0 {
            break;
        }
        input.read_exact(&mut size[1..])?;
        let len = u32::from_be_bytes(size) as usize;
        if len == 0 || len > 64 * 1024 {
            return Err(invalid("blob header length"));
        }
        let mut bytes = vec![0; len];
        input.read_exact(&mut bytes)?;
        let f = fields(&bytes)?;
        let kind = text(blob(&f, 1)?)?;
        let size = int(&f, 3, None)?;
        if size == 0 || size > BLOCK as u64 {
            return Err(Error::LimitExceeded);
        }
        let mut bytes = vec![0; size as usize];
        input.read_exact(&mut bytes)?;
        let f = fields(&bytes)?;
        if f.iter()
            .filter(|(id, _)| matches!(id, 1 | 3 | 4 | 5 | 6 | 7))
            .count()
            != 1
        {
            return Err(invalid("blob needs exactly one payload"));
        }
        let raw = if let Ok(raw) = blob(&f, 1) {
            raw.to_vec()
        } else {
            let expected = int(&f, 2, None)?;
            if expected > BLOCK as u64 {
                return Err(Error::LimitExceeded);
            }
            let mut raw = Vec::new();
            ZlibDecoder::new(blob(&f, 3)?)
                .take(BLOCK as u64 + 1)
                .read_to_end(&mut raw)?;
            if raw.len() as u64 != expected {
                return Err(invalid("decompressed size"));
            }
            raw
        };
        decoded = decoded.checked_add(raw.len()).ok_or(Error::LimitExceeded)?;
        if decoded > 512 * 1024 * 1024 {
            return Err(Error::LimitExceeded);
        }
        match kind.as_str() {
            "OSMHeader" => {
                if header {
                    return Err(invalid("duplicate header"));
                }
                header = true;
                let f = fields(&raw)?;
                if !f
                    .iter()
                    .any(|(id, v)| *id == 4 && matches!(v,Value::Bytes(b) if *b==b"OsmSchema-V0.6"))
                {
                    return Err(invalid("missing required schema feature"));
                }
                for (_, v) in f.iter().filter(|(k, _)| *k == 4) {
                    let Value::Bytes(b) = v else {
                        return Err(invalid("feature"));
                    };
                    if !matches!(*b, b"OsmSchema-V0.6" | b"DenseNodes") {
                        return Err(invalid(format!(
                            "unsupported required feature: {}",
                            text(b)?
                        )));
                    }
                }
                osm.snapshot = int(&f, 32, None).ok().map(|v| v as i64);
            }
            "OSMData" => {
                if !header {
                    return Err(invalid("missing OSM header"));
                }
                primitive(&raw, &mut osm)?;
            }
            _ => return Err(invalid("unsupported block")),
        }
    }
    if !header || osm.nodes.is_empty() {
        return Err(invalid("empty PBF"));
    }
    Ok(osm)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_overflows_and_truncation_without_panicking() {
        for bytes in [
            vec![0x80],
            vec![0xff; 11],
            vec![
                0x0a, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2,
            ],
            vec![0x0a, 10, 1],
            vec![0],
        ] {
            assert!(fields(&bytes).is_err());
        }
        assert!(delta(vec![u64::MAX - 1, 4]).is_err());
        assert!(point(i64::MAX, 0, 100, 0, 0).is_err());
        assert!(primitive(&[0x0a, 0], &mut Osm::default()).is_ok());
    }
}
