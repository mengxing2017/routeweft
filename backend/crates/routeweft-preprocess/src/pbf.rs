//! Bounded PBF wire decoder written against the public OSM-binary schema.
//! Supports raw/zlib blocks and ordinary/dense nodes; no copied protobuf code.
use flate2::read::ZlibDecoder;
use routeweft_dataset::{invalid, resource_limit, Error, Point, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;
pub type Tags = BTreeMap<String, String>;
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Node {
    pub id: i64,
    pub point: Point,
    pub tags: Tags,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Way {
    pub id: i64,
    pub refs: Vec<i64>,
    pub tags: Tags,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Member {
    pub id: i64,
    pub kind: u64,
    pub role: String,
}
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct Relation {
    pub id: i64,
    pub members: Vec<Member>,
    pub tags: Tags,
}
/// A decoder destination. Returning an error stops decoding immediately.
/// Implementations must perform their own global duplicate-ID validation.
pub(crate) trait RecordSink {
    fn node(&mut self, node: Node) -> Result<()>;
    fn way(&mut self, way: Way) -> Result<()>;
    fn relation(&mut self, relation: Relation) -> Result<()>;
}
#[derive(Default)]
pub struct Osm {
    pub nodes: BTreeMap<i64, Node>,
    pub ways: BTreeMap<i64, Way>,
    pub relations: Vec<Relation>,
    pub snapshot: Option<i64>,
    estimated_bytes: usize,
    relation_ids: BTreeSet<i64>,
    scan_only: bool,
    driving_projection: bool,
    counts: [u64; 3],
    decoded_bytes: u64,
    streaming: bool,
}
impl Osm {
    fn charge(&mut self, bytes: usize) -> Result<()> {
        let observed = self.counts.iter().sum::<u64>() + 1;
        let objects = if self.streaming {
            STREAM_OBJECTS
        } else {
            OBJECTS as u64
        };
        if observed > objects {
            return Err(resource_limit(
                "osm_objects",
                objects,
                observed,
                "pbf_decode",
            ));
        }
        self.estimated_bytes = self
            .estimated_bytes
            .checked_add(bytes)
            .ok_or(Error::LimitExceeded)?;
        // Streaming releases each record after the sink call. This counter is
        // cumulative work, not retained memory; batches and records are bounded
        // independently by the destination. In-memory decode keeps its old cap.
        let bytes = if self.streaming {
            STREAM_OBJECT_BYTES
        } else {
            512 * 1024 * 1024
        };
        if self.estimated_bytes as u64 > bytes {
            return Err(resource_limit(
                if self.streaming {
                    "osm_work_bytes"
                } else {
                    "estimated_osm_bytes"
                },
                bytes,
                self.estimated_bytes as u64,
                "pbf_decode",
            ));
        }
        Ok(())
    }
}
fn tag_bytes(tags: &Tags) -> usize {
    tags.iter().map(|(k, v)| k.len() + v.len() + 96).sum()
}
const BLOCK: usize = 32 * 1024 * 1024;
const OBJECTS: usize = 2_000_000;
// External decode work limits, independent of resident-object capacity.
// These are finite processing allowances, not a province/3 GiB support claim.
pub const STREAM_OBJECTS: u64 = 100_000_000;
pub const STREAM_OBJECT_BYTES: u64 = 64 * 1024 * 1024 * 1024;
pub const STREAM_DECODED_BYTES: u64 = 16 * 1024 * 1024 * 1024;
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
            return Err(resource_limit(
                "protobuf_fields",
                1_000_000,
                out.len() as u64 + 1,
                "pbf_decode",
            ));
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
                        return Err(resource_limit(
                            "packed_values",
                            OBJECTS as u64,
                            out.len() as u64,
                            "pbf_decode",
                        ));
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
        return Err(resource_limit(
            "string_bytes",
            65536,
            bytes.len() as u64,
            "pbf_decode",
        ));
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
// Keep this projection in sync with graph_attempt's driving profile. Decode and
// charge the complete object first; unused data must not hide malformed input.
fn retain_tags(tags: &mut Tags, keep: impl Fn(&str) -> bool) {
    tags.retain(|key, _| keep(key));
    // retain() can leave an allocated empty tree root behind.
    if tags.is_empty() {
        *tags = Tags::new();
    }
}
fn driving_way_tag(key: &str) -> bool {
    matches!(
        key,
        "highway" | "name" | "area" | "service" | "reversible" | "oneway" | "junction"
    ) || key.contains(":conditional")
        || [
            "motorcar",
            "motor_vehicle",
            "vehicle",
            "access",
            "maxspeed",
            "maxheight",
            "maxwidth",
            "maxweight",
            "maxlength",
            "maxaxleload",
        ]
        .iter()
        .any(|base| {
            key == *base
                || key
                    .strip_prefix(base)
                    .is_some_and(|suffix| suffix.starts_with(':'))
        })
}
#[cfg(test)]
fn insert_way(osm: &mut Osm, way: Way) -> Result<()> {
    emit_way(osm, way, &mut None)
}
fn emit_way(osm: &mut Osm, mut way: Way, sink: &mut Option<&mut dyn RecordSink>) -> Result<()> {
    osm.charge(128 + way.refs.len() * 8 + tag_bytes(&way.tags))?;
    osm.counts[1] += 1;
    if osm.scan_only && sink.is_none() {
        return Ok(());
    }
    if osm.driving_projection {
        retain_tags(&mut way.tags, driving_way_tag);
    }
    if let Some(sink) = sink.as_deref_mut() {
        return sink.way(way);
    }
    // Even unsupported ways retain their references: restriction diagnostics
    // distinguish missing nodes from roads excluded by the profile.
    if osm.ways.insert(way.id, way).is_some() {
        return Err(invalid("duplicate way"));
    }
    Ok(())
}
fn project_relation(relation: &mut Relation) {
    // All restriction payloads remain intact, including unsupported/excepted
    // modes, so conservative resolution and its diagnostics cannot lose data.
    if crate::value(&relation.tags, "type") != "restriction" {
        relation.members = Vec::new();
        retain_tags(&mut relation.tags, |key| key == "type");
    }
}
#[cfg(test)]
fn insert_node(osm: &mut Osm, node: Node) -> Result<()> {
    emit_node(osm, node, &mut None)
}
fn emit_node(osm: &mut Osm, mut node: Node, sink: &mut Option<&mut dyn RecordSink>) -> Result<()> {
    osm.charge(128 + tag_bytes(&node.tags))?;
    osm.counts[0] += 1;
    if osm.scan_only && sink.is_none() {
        return Ok(());
    }
    if osm.driving_projection {
        retain_tags(&mut node.tags, |key| {
            matches!(
                key,
                "motorcar" | "motor_vehicle" | "vehicle" | "access" | "barrier"
            ) || key.contains(":conditional")
        });
    }
    if let Some(sink) = sink.as_deref_mut() {
        return sink.node(node);
    }
    if osm.nodes.insert(node.id, node).is_some() {
        return Err(invalid("duplicate node"));
    }
    Ok(())
}
#[cfg(test)]
fn primitive(bytes: &[u8], osm: &mut Osm) -> Result<()> {
    primitive_sink(bytes, osm, &mut None)
}
fn primitive_sink(
    bytes: &[u8],
    osm: &mut Osm,
    sink: &mut Option<&mut dyn RecordSink>,
) -> Result<()> {
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
                    emit_node(osm, node, sink)?;
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
                                    return Err(resource_limit(
                                        "node_tags",
                                        4096,
                                        tags.len() as u64,
                                        "pbf_decode",
                                    ));
                                }
                            }
                        }
                        emit_node(
                            osm,
                            Node {
                                id,
                                point: point(lon, lat, gran, lon_offset, lat_offset)?,
                                tags,
                            },
                            sink,
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
                    emit_way(osm, way, sink)?;
                }
                4 => {
                    let relation_id = int(&v, 1, None)? as i64;
                    if !osm.scan_only && !osm.relation_ids.insert(relation_id) {
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
                    osm.counts[2] += 1;
                    if !osm.scan_only || sink.is_some() {
                        let mut relation = Relation {
                            id: relation_id,
                            members,
                            tags,
                        };
                        if osm.driving_projection {
                            project_relation(&mut relation);
                        }
                        if let Some(sink) = sink.as_deref_mut() {
                            sink.relation(relation)?;
                        } else {
                            osm.relations.push(relation);
                        }
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}
/// Same bounded wire decoder, without retaining the global object collections.
/// This does not resolve references, detect duplicate IDs, or validate driving rules.
#[derive(Debug, serde::Serialize)]
pub struct StructureReport {
    pub validation_scope: &'static str,
    pub semantic_validation: bool,
    pub duplicate_ids_checked: bool,
    pub source_sha256: String,
    pub source_snapshot: Option<i64>,
    pub nodes: u64,
    pub ways: u64,
    pub relations: u64,
    pub decoded_bytes: u64,
    /// Estimate of what the full decoder would retain, not this scanner's RSS.
    pub estimated_full_decode_bytes: usize,
}
pub fn scan(path: &Path) -> Result<StructureReport> {
    let input_bytes = std::fs::metadata(path)?.len();
    if input_bytes > 512 * 1024 * 1024 {
        return Err(resource_limit(
            "input_bytes",
            512 * 1024 * 1024,
            input_bytes,
            "pbf_read",
        ));
    }
    let source_sha256 = routeweft_dataset::hash_file(path)?;
    let osm = read_mode(path, true, false)?;
    if routeweft_dataset::hash_file(path)? != source_sha256 {
        return Err(invalid("input changed while scanning"));
    }
    Ok(StructureReport {
        validation_scope: "structure_only",
        semantic_validation: false,
        duplicate_ids_checked: false,
        source_sha256,
        source_snapshot: osm.snapshot,
        nodes: osm.counts[0],
        ways: osm.counts[1],
        relations: osm.counts[2],
        decoded_bytes: osm.decoded_bytes,
        estimated_full_decode_bytes: osm.estimated_bytes,
    })
}
pub fn read(path: &Path) -> Result<Osm> {
    read_mode(path, false, false)
}
// Build-only projection; decoding, validation and full-object resource accounting
// happen before unused tags and non-restriction members are released.
// Public read remains lossless.
pub(crate) fn read_for_driving(path: &Path) -> Result<Osm> {
    read_mode(path, false, true)
}
fn read_mode(path: &Path, scan_only: bool, driving_projection: bool) -> Result<Osm> {
    decode(path, scan_only, driving_projection, None)
}
/// Decode projected records without retaining global maps or identity sets.
pub(crate) fn read_to_sink(
    path: &Path,
    sink: &mut dyn RecordSink,
) -> Result<([u64; 3], Option<i64>)> {
    let osm = decode(path, true, true, Some(sink))?;
    Ok((osm.counts, osm.snapshot))
}
fn decode(
    path: &Path,
    scan_only: bool,
    driving_projection: bool,
    mut sink: Option<&mut dyn RecordSink>,
) -> Result<Osm> {
    let mut input = std::fs::File::open(path)?;
    if input.metadata()?.len() > 512 * 1024 * 1024 {
        return Err(resource_limit(
            "input_bytes",
            512 * 1024 * 1024,
            input.metadata()?.len(),
            "pbf_read",
        ));
    }
    let streaming = sink.is_some();
    let mut osm = Osm {
        streaming,
        scan_only,
        driving_projection,
        ..Default::default()
    };
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
            return Err(resource_limit("blob_bytes", BLOCK as u64, size, "pbf_read"));
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
                return Err(resource_limit(
                    "raw_block_bytes",
                    BLOCK as u64,
                    expected,
                    "pbf_decode",
                ));
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
        let decoded_limit = if streaming {
            STREAM_DECODED_BYTES
        } else {
            512 * 1024 * 1024
        };
        if decoded as u64 > decoded_limit {
            return Err(resource_limit(
                "decoded_bytes",
                decoded_limit,
                decoded as u64,
                "pbf_decode",
            ));
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
                primitive_sink(&raw, &mut osm, &mut sink)?;
            }
            _ => return Err(invalid("unsupported block")),
        }
    }
    if !header || osm.counts[0] == 0 {
        return Err(invalid("empty PBF"));
    }
    osm.decoded_bytes = decoded as u64;
    Ok(osm)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Discard;
    impl RecordSink for Discard {
        fn node(&mut self, _: Node) -> Result<()> {
            Ok(())
        }
        fn way(&mut self, _: Way) -> Result<()> {
            Ok(())
        }
        fn relation(&mut self, _: Relation) -> Result<()> {
            Ok(())
        }
    }
    #[test]
    fn way_projection_preserves_profile_restrictions_and_full_budget() {
        for (key, val) in [
            ("source", "discard"),
            ("name", "Keep Road Name"),
            ("highway", "footway"),
            ("area", "yes"),
            ("service", "emergency_access"),
            ("reversible", "yes"),
            ("oneway", "-1"),
            ("oneway", "unsupported"),
            ("junction", "roundabout"),
            ("access", "no"),
            ("access:forward", "no"),
            ("vehicle:backward", "private"),
            ("motor_vehicle:forward", "no"),
            ("motorcar:backward", "no"),
            ("maxspeed", "20 mph"),
            ("maxspeed:forward", "10"),
            ("maxspeed:backward", "invalid"),
            ("maxheight:physical", "3"),
            ("maxwidth", "2"),
            ("maxweight:hgv", "7"),
            ("maxlength", "5"),
            ("maxaxleload", "4"),
            ("unexpected:conditional:suffix", "no @ (Mo)"),
        ] {
            let mut full = Osm::default();
            let mut projected = Osm {
                driving_projection: true,
                ..Default::default()
            };
            for osm in [&mut full, &mut projected] {
                for id in 1..=3 {
                    insert_node(
                        osm,
                        Node {
                            id,
                            point: Point {
                                lon: id as f64 * 0.001,
                                lat: 0.,
                            },
                            tags: Tags::new(),
                        },
                    )
                    .unwrap();
                }
                for (id, refs) in [(10, vec![1, 2]), (20, vec![2, 3])] {
                    let mut tags = Tags::from([
                        ("highway".into(), "residential".into()),
                        ("source".into(), "survey".into()),
                    ]);
                    if id == 10 {
                        tags.insert(key.into(), val.into());
                    }
                    insert_way(osm, Way { id, refs, tags }).unwrap();
                }
                osm.relations.push(Relation {
                    id: 100,
                    tags: Tags::from([
                        ("type".into(), "restriction".into()),
                        ("restriction".into(), "no_straight_on".into()),
                    ]),
                    members: [(10, 1, "from"), (2, 0, "via"), (20, 1, "to")]
                        .into_iter()
                        .map(|(id, kind, role)| Member {
                            id,
                            kind,
                            role: role.into(),
                        })
                        .collect(),
                });
            }
            assert_eq!(full.estimated_bytes, projected.estimated_bytes);
            assert_eq!(full.ways[&10].refs, projected.ways[&10].refs);
            assert!(!projected.ways[&20].tags.contains_key("source"));
            for policy in [
                crate::InvalidRestrictionPolicy::Reject,
                crate::InvalidRestrictionPolicy::BlockWays,
            ] {
                let options = crate::BuildOptions {
                    invalid_restrictions: policy,
                    ..Default::default()
                };
                assert_eq!(
                    format!("{:?}", crate::prepare(&full, options)),
                    format!("{:?}", crate::prepare(&projected, options)),
                    "{key}={val}"
                );
            }
            assert!(insert_way(
                &mut projected,
                Way {
                    id: 10,
                    refs: vec![],
                    tags: Tags::new()
                }
            )
            .is_err());
        }
        // Dropped tags must still trip the original full-object memory budget.
        for projected in [false, true] {
            let mut osm = Osm {
                driving_projection: projected,
                estimated_bytes: 512 * 1024 * 1024 - 128,
                ..Default::default()
            };
            assert!(matches!(
                insert_way(
                    &mut osm,
                    Way {
                        id: 1,
                        refs: vec![],
                        tags: Tags::from([("source".into(), "survey".into())])
                    }
                ),
                Err(Error::ResourceLimit(_))
            ));
        }
    }
    #[test]
    fn nonrestriction_payload_is_validated_before_release() {
        fn bytes(field: u8, payload: &[u8]) -> Vec<u8> {
            let mut result = vec![field * 8 + 2, payload.len() as u8];
            result.extend(payload);
            result
        }
        let table = [
            bytes(1, b""),
            bytes(1, b"type"),
            bytes(1, b"multipolygon"),
            bytes(1, b"restriction"),
            bytes(1, b"name"),
            bytes(1, b"unused"),
        ]
        .concat();
        for restriction in [false, true] {
            for failure in 0..4 {
                let relation = [
                    vec![8, 1],
                    bytes(2, &[1, 4]),
                    bytes(3, &[if restriction { 3 } else { 2 }, 5]),
                    bytes(
                        8,
                        if failure == 1 {
                            &[99]
                        } else if failure == 2 {
                            &[]
                        } else {
                            &[5]
                        },
                    ),
                    bytes(9, &[2]),
                    bytes(10, if failure == 3 { &[3] } else { &[1] }),
                ]
                .concat();
                let block = [bytes(1, &table), bytes(2, &bytes(4, &relation))].concat();
                let mut full = Osm::default();
                let mut projected = Osm {
                    driving_projection: true,
                    ..Default::default()
                };
                assert_eq!(primitive(&block, &mut full).is_ok(), failure == 0);
                assert_eq!(primitive(&block, &mut projected).is_ok(), failure == 0);
                let mut accounting = Osm {
                    scan_only: true,
                    driving_projection: true,
                    ..Default::default()
                };
                assert_eq!(
                    primitive_sink(&block, &mut accounting, &mut Some(&mut Discard)).is_ok(),
                    failure == 0
                );
                assert!(accounting.relations.is_empty() && accounting.relation_ids.is_empty());
                if failure == 0 {
                    assert_eq!(full.estimated_bytes, projected.estimated_bytes);
                    assert_eq!(full.relations[0].members.len(), 1);
                    assert_eq!(
                        projected.relations[0].members.len(),
                        usize::from(restriction)
                    );
                    assert_eq!(
                        projected.relations[0].tags.contains_key("name"),
                        restriction
                    );
                    assert!(primitive(&block, &mut projected).is_err());
                }
            }
        }
    }
    #[test]
    fn projection_validates_normal_and_dense_tag_references_before_discarding() {
        fn bytes(field: u8, payload: &[u8]) -> Vec<u8> {
            let mut result = vec![field * 8 + 2, payload.len() as u8];
            result.extend(payload);
            result
        }
        let table = [bytes(1, b""), bytes(1, b"name"), bytes(1, b"label")].concat();
        for dense in [false, true] {
            for valid in [false, true] {
                let reference = if valid { 2 } else { 99 };
                let node = if dense {
                    [
                        bytes(1, &[2]),
                        bytes(8, &[0]),
                        bytes(9, &[0]),
                        bytes(10, &[1, reference, 0]),
                    ]
                    .concat()
                } else {
                    [
                        vec![8, 2, 64, 0, 72, 0],
                        bytes(2, &[1]),
                        bytes(3, &[reference]),
                    ]
                    .concat()
                };
                let primitive_block = [
                    bytes(1, &table),
                    bytes(2, &bytes(if dense { 2 } else { 1 }, &node)),
                ]
                .concat();
                let mut full = Osm::default();
                let mut projected = Osm {
                    driving_projection: true,
                    ..Default::default()
                };
                assert_eq!(primitive(&primitive_block, &mut full).is_ok(), valid);
                assert_eq!(primitive(&primitive_block, &mut projected).is_ok(), valid);
                let mut accounting = Osm {
                    scan_only: true,
                    driving_projection: true,
                    ..Default::default()
                };
                assert_eq!(
                    primitive_sink(&primitive_block, &mut accounting, &mut Some(&mut Discard))
                        .is_ok(),
                    valid
                );
                assert!(accounting.nodes.is_empty());
                if valid {
                    assert_eq!(full.nodes[&1].tags["name"], "label");
                    assert!(projected.nodes[&1].tags.is_empty());
                    assert_eq!(full.estimated_bytes, projected.estimated_bytes);
                }
            }
        }
    }
    #[test]
    fn driving_projection_preserves_profile_decisions_and_full_budget() {
        for (key, val) in [
            ("name", "discard me"),
            ("access", "no"),
            ("motorcar", "yes"),
            ("motor_vehicle", "private"),
            ("vehicle", "no"),
            ("barrier", "bollard"),
            ("barrier", "entrance"),
            ("unexpected:conditional:suffix", "yes @ (Mo)"),
        ] {
            let mut full = Osm::default();
            let mut projected = Osm {
                driving_projection: true,
                ..Default::default()
            };
            for osm in [&mut full, &mut projected] {
                for id in 1..=3 {
                    let mut tags = Tags::from([("source".into(), "survey".into())]);
                    if id == 2 {
                        tags.insert(key.into(), val.into());
                    }
                    insert_node(
                        osm,
                        Node {
                            id,
                            point: Point {
                                lon: id as f64 * 0.001,
                                lat: 0.,
                            },
                            tags,
                        },
                    )
                    .unwrap();
                }
                osm.ways.insert(
                    10,
                    Way {
                        id: 10,
                        refs: vec![1, 2, 3],
                        tags: Tags::from([("highway".into(), "residential".into())]),
                    },
                );
            }
            assert_eq!(full.estimated_bytes, projected.estimated_bytes);
            assert!(full.nodes[&1].tags.contains_key("source"));
            assert!(projected.nodes[&1].tags.is_empty());
            assert_eq!(
                format!("{:?}", crate::graph(&full)),
                format!("{:?}", crate::graph(&projected))
            );
            assert!(insert_node(
                &mut projected,
                Node {
                    id: 1,
                    point: Point { lon: 0., lat: 0. },
                    tags: Tags::new(),
                }
            )
            .is_err());
        }
    }
    #[test]
    fn structural_scan_discards_objects_and_rejects_full_decode_capacity() {
        let mut scan = Osm {
            scan_only: true,
            ..Default::default()
        };
        for id in 0..1000 {
            insert_node(
                &mut scan,
                Node {
                    id,
                    point: Point { lon: 0., lat: 0. },
                    tags: Tags::new(),
                },
            )
            .unwrap();
        }
        assert_eq!(scan.counts, [1000, 0, 0]);
        assert!(scan.nodes.is_empty());
        assert!(scan.ways.is_empty());
        assert!(scan.relations.is_empty());
        scan.counts[0] = OBJECTS as u64;
        let error = insert_node(
            &mut scan,
            Node {
                id: 1001,
                point: Point { lon: 0., lat: 0. },
                tags: Tags::new(),
            },
        )
        .unwrap_err();
        let Error::ResourceLimit(resource) = error else {
            panic!("missing resource diagnostic")
        };
        assert_eq!(resource.resource, "osm_objects");
        assert_eq!(resource.observed, OBJECTS as u64 + 1);
        assert_eq!(resource.stage, "pbf_decode");
    }
    #[test]
    fn streaming_work_is_independent_of_resident_capacity_and_still_bounded() {
        let mut osm = Osm {
            scan_only: true,
            streaming: true,
            counts: [OBJECTS as u64, 0, 0],
            estimated_bytes: 512 * 1024 * 1024,
            ..Default::default()
        };
        emit_node(
            &mut osm,
            Node {
                id: 1,
                point: Point { lon: 0., lat: 0. },
                tags: Tags::new(),
            },
            &mut Some(&mut Discard),
        )
        .unwrap();
        assert_eq!(osm.counts[0], OBJECTS as u64 + 1);
        assert!(osm.nodes.is_empty() && osm.relation_ids.is_empty());
        osm.counts = [STREAM_OBJECTS, 0, 0];
        let Error::ResourceLimit(resource) = osm.charge(128).unwrap_err() else {
            panic!("missing object limit")
        };
        assert_eq!(resource.resource, "osm_objects");
        assert_eq!(resource.limit, STREAM_OBJECTS);
        assert_eq!(resource.observed, STREAM_OBJECTS + 1);
        osm.counts = [0; 3];
        osm.estimated_bytes = STREAM_OBJECT_BYTES as usize;
        let Error::ResourceLimit(resource) = osm.charge(128).unwrap_err() else {
            panic!("missing work limit")
        };
        assert_eq!(resource.resource, "osm_work_bytes");
        assert_eq!(resource.limit, STREAM_OBJECT_BYTES);
        assert_eq!(resource.observed, STREAM_OBJECT_BYTES + 128);
    }
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
