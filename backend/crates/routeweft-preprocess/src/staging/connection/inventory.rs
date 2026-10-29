//! Full source restriction inventory; independent of the bounded failure audit.
//! Only dependency objects are retained, under an explicit logical memory cap.
use super::{Record, Source};
use crate::{
    pbf::{Node, Osm, Relation},
    restrictions, InvalidMaxspeedPolicy, Result,
};
use routeweft_dataset::{hash_file, invalid, resource_limit, Adjacency};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufWriter, Write};
use std::path::Path;

#[derive(Serialize)]
pub struct InventorySummary {
    pub format_version: u32,
    pub complete: bool,
    pub source_sha256: String,
    pub source_snapshot: Option<i64>,
    pub restrictions: u64,
    pub applicable: u64,
    pub classifications: BTreeMap<String, u64>,
    pub estimated_retained_bytes: u64,
    pub memory_limit_bytes: u64,
    pub inventory_sha256: String,
}
fn charge(total: &mut u64, bytes: u64, limit: u64) -> Result<()> {
    *total = total.saturating_add(bytes);
    if *total > limit {
        return Err(resource_limit(
            "restriction_inventory_memory",
            limit,
            *total,
            "restriction_inventory",
        )
        .into());
    }
    Ok(())
}
fn insert(set: &mut BTreeSet<i64>, id: i64, memory: &mut u64, limit: u64) -> Result<()> {
    if !set.contains(&id) {
        charge(memory, 128, limit)?;
        set.insert(id);
    }
    Ok(())
}
fn shape_issues(rel: &Relation) -> Vec<&'static str> {
    let mut issues = Vec::new();
    let from: Vec<_> = rel.members.iter().filter(|m| m.role == "from").collect();
    let to: Vec<_> = rel.members.iter().filter(|m| m.role == "to").collect();
    let via: Vec<_> = rel.members.iter().filter(|m| m.role == "via").collect();
    if from.is_empty() || to.is_empty() {
        issues.push("MissingFromTo");
    }
    if via.is_empty() {
        issues.push("MissingVia");
    }
    if rel.members.iter().any(|m| m.role.is_empty()) {
        issues.push("EmptyMemberRole");
    }
    if rel.members.iter().any(|m| m.kind > 1) {
        issues.push("UnboundedMemberDependency");
    }
    if from.iter().chain(&to).any(|m| m.kind != 1) {
        issues.push("InvalidMemberType");
    }
    issues
}
/// Audit a validated stage. `output` is a new JSONL file. A caller must only mark
/// the containing artifact complete after this function and final source checks.
/// Fixtures are dependency-complete *when the source is*; missing IDs are reported.
pub fn audit_staged_restrictions(
    staged: &Path,
    output: &Path,
    fixtures: &Path,
    memory_limit_bytes: u64,
) -> Result<InventorySummary> {
    if !(1024 * 1024..=512 * 1024 * 1024).contains(&memory_limit_bytes) {
        return Err(invalid("inventory memory limit must be 1-512 MiB").into());
    }
    let source = Source::open(staged, 4 * 1024 * 1024)?;
    let mut member_ways = BTreeSet::new();
    let mut needed_nodes = BTreeSet::new();
    let mut conditional_ways = BTreeSet::new();
    let mut memory = 0;
    source.visit_kinds(&[2], |record| {
        if let Record::Relation(rel) = record {
            if crate::value(&rel.tags, "type") == "restriction" {
                for m in &rel.members {
                    if m.kind == 1 {
                        insert(&mut member_ways, m.id, &mut memory, memory_limit_bytes)?;
                        if restrictions::applies(&rel) && restrictions::conditional(&rel) {
                            insert(&mut conditional_ways, m.id, &mut memory, memory_limit_bytes)?;
                        }
                    } else if m.kind == 0 {
                        insert(&mut needed_nodes, m.id, &mut memory, memory_limit_bytes)?;
                    }
                }
            }
        }
        Ok(())
    })?;
    let mut dependencies = Osm::default();
    source.visit_kinds(&[1], |record| {
        if let Record::Way(way) = record {
            if member_ways.contains(&way.id) {
                let bytes = serde_json::to_vec(&way)
                    .map_err(|e| invalid(e.to_string()))?
                    .len() as u64;
                charge(&mut memory, bytes * 4 + 512, memory_limit_bytes)?;
                for id in &way.refs {
                    insert(&mut needed_nodes, *id, &mut memory, memory_limit_bytes)?;
                }
                dependencies.ways.insert(way.id, way);
            }
        }
        Ok(())
    })?;
    source.visit_kinds(&[0], |record| {
        if let Record::Node {
            id,
            lon_bits,
            lat_bits,
            tags,
        } = record
        {
            if needed_nodes.contains(&id) {
                let tag_bytes: usize = tags.iter().map(|(k, v)| k.len() + v.len() + 128).sum();
                charge(&mut memory, 256 + tag_bytes as u64 * 2, memory_limit_bytes)?;
                dependencies.nodes.insert(
                    id,
                    Node {
                        id,
                        point: routeweft_dataset::Point {
                            lon: f64::from_bits(lon_bits),
                            lat: f64::from_bits(lat_bits),
                        },
                        tags,
                    },
                );
            }
        }
        Ok(())
    })?;
    let mut summary = InventorySummary {
        format_version: 1,
        complete: false,
        source_sha256: source.report.source_sha256.clone(),
        source_snapshot: source.report.source_snapshot,
        restrictions: 0,
        applicable: 0,
        classifications: BTreeMap::new(),
        estimated_retained_bytes: memory,
        memory_limit_bytes,
        inventory_sha256: String::new(),
    };
    fs::create_dir(fixtures)?;
    let mut writer = BufWriter::new(File::options().write(true).create_new(true).open(output)?);
    source.visit_kinds(&[2], |record| {
        let Record::Relation(rel) = record else { return Ok(()); };
        if crate::value(&rel.tags, "type") != "restriction" { return Ok(()); }
        summary.restrictions += 1;
        let applicable = restrictions::applies(&rel);
        summary.applicable += applicable as u64;
        let mut local = Osm::default();
        local.snapshot = source.report.source_snapshot;
        let mut issues = shape_issues(&rel);
        let mut states = Vec::new();
        let mut retained = BTreeSet::new();
        for m in &rel.members {
            match m.kind {
                0 => {
                    if let Some(n) = dependencies.nodes.get(&m.id) {
                        local.nodes.insert(m.id, n.clone());
                    } else { issues.push("MissingSourceNode"); }
                },
                1 => {
                    if let Some(w) = dependencies.ways.get(&m.id) {
                        let mut stats = BTreeMap::new();
                        let selection = crate::select_way(w, conditional_ways.contains(&w.id),
                            &mut stats, InvalidMaxspeedPolicy::UseDefault, &mut Vec::new());
                        let (selected, profile_error) = match selection {
                            Ok(s) => (s.is_some(), None), Err(e) => (false, Some(e.to_string())),
                        };
                        let missing: Vec<_> = w.refs.iter().filter(|id| !dependencies.nodes.contains_key(id)).copied().collect();
                        if !missing.is_empty() { issues.push("MissingSourceWayNode"); }
                        if selected && missing.is_empty() { retained.insert(w.id); }
                        states.push(serde_json::json!({"way_id":w.id,"source_present":true,
                            "reference_complete":missing.is_empty(),"missing_node_ids":missing,
                            "profile_selected":selected,"profile_error":profile_error,"profile_diagnostics":stats}));
                        local.ways.insert(w.id, w.clone());
                        for id in &w.refs {
                            if let Some(n) = dependencies.nodes.get(id) { local.nodes.insert(*id,n.clone()); }
                        }
                    } else {
                        issues.push("MissingSourceWay");
                        states.push(serde_json::json!({"way_id":m.id,"source_present":false}));
                    }
                },
                _ => (),
            }
        }
        let mut classification = "not_applicable".to_owned();
        let mut diagnostic = None;
        let mut paths = 0;
        if applicable {
            if restrictions::conditional(&rel) { classification = "conditional_ways_excluded".into(); }
            else if retained.is_empty() { classification = "no_retained_member_way".into(); }
            else {
                // Build the member-way subgraph only. OSM paths in this relation
                // cannot use non-member ways; no turn is inferred from geometry.
                let excluded = local.ways.keys().filter(|id| !retained.contains(id)).copied().collect();
                match crate::graph_attempt(&local, &excluded, InvalidMaxspeedPolicy::UseDefault, &mut Vec::new()) {
                    Ok((graph,_)) => {
                        let mapping: Vec<_> = retained.iter().flat_map(|id| local.ways[id].refs.iter().copied())
                            .collect::<BTreeSet<_>>().into_iter().collect();
                        let mut by_way: BTreeMap<i64,Vec<usize>> = BTreeMap::new();
                        for (id,e) in graph.edges.iter().enumerate() { by_way.entry(e.way).or_default().push(id); }
                        let adjacency = Adjacency::new(graph.nodes.len(), &graph.edges)?;
                        match restrictions::resolve(&rel, &local, &mapping, &by_way, &graph.edges, &adjacency) {
                            Ok(Some((_,sequences))) => { classification="resolved".into(); paths=sequences.len(); },
                            Ok(None) => classification="no_retained_member_way".into(),
                            Err(d) => { classification=d.code.clone(); diagnostic=Some(d); },
                        }
                    },
                    Err(error) if matches!(&error.source, routeweft_dataset::Error::InvalidDataset(message) if message == "no driving roads") => {
                        classification="no_retained_member_way".into();
                        issues.push("NoEffectiveDirectedEdges");
                    },
                    Err(error) => { classification="member_graph_error".into(); issues.push("MemberGraphBuildFailed");
                        states.push(serde_json::json!({"graph_error":error.to_string()})); },
                }
            }
        }
        issues.sort_unstable(); issues.dedup();
        *summary.classifications.entry(classification.clone()).or_default() += 1;
        let record = serde_json::json!({"relation_id":rel.id,"applicable":applicable,
            "classification":classification,"issues":issues,"diagnostic":diagnostic,
            "directed_path_count":paths,"relation":rel,"way_states":states,
            "topology_scope":"member_way_subgraph; other relations are not compiled"});
        serde_json::to_writer(&mut writer,&record).map_err(|e| invalid(e.to_string()))?;
        writer.write_all(b"\n")?;
        if [5652903,5652904,5652906,1991548,2260442,19417084].contains(&rel.id) {
            // Lossless JSON fixture for the projected driving source objects,
            // OSM XML conversion can be performed separately without guessing roles.
            local.relations.push(rel);
            let path = fixtures.join(format!("{}.json",local.relations[0].id));
            let mut file = BufWriter::new(File::options().write(true).create_new(true).open(path)?);
            serde_json::to_writer(&mut file,&serde_json::json!({"nodes":local.nodes,"ways":local.ways,
                "relations":local.relations,"source_sha256":summary.source_sha256}))
                .map_err(|e| invalid(e.to_string()))?;
            file.flush()?;
        }
        Ok(())
    })?;
    writer.flush()?;
    writer.get_ref().sync_all()?;
    summary.inventory_sha256 = hash_file(output)?;
    summary.complete = true;
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pbf::{Member, Tags, Way};
    use crate::staging::connection::tests::{fixture, source, temporary};
    #[test]
    fn inventory_distinguishes_profile_selection_from_effective_edges() {
        let parent = temporary();
        let mut osm = fixture();
        osm.relations.clear();
        osm.ways.clear();
        osm.ways.insert(
            10,
            Way {
                id: 10,
                refs: vec![-3, 2],
                tags: Tags::from([("highway".into(), "residential".into())]),
            },
        );
        osm.relations.push(Relation {
            id: 2,
            tags: Tags::from([
                ("type".into(), "restriction".into()),
                ("restriction".into(), "no_right_turn".into()),
            ]),
            members: vec![Member {
                id: 10,
                kind: 1,
                role: "from".into(),
            }],
        });
        let staged = source(&parent, &osm);
        let output = parent.join("inventory.jsonl");
        let summary =
            audit_staged_restrictions(&staged, &output, &parent.join("fixtures"), 8 * 1024 * 1024)
                .unwrap();
        assert_eq!(summary.classifications["no_retained_member_way"], 1);
        let row: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(output).unwrap()).unwrap();
        assert_eq!(row["way_states"][0]["profile_selected"], true);
        assert!(row["issues"]
            .as_array()
            .unwrap()
            .contains(&serde_json::json!("NoEffectiveDirectedEdges")));
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn inventory_separates_missing_filtered_and_multiple_shape_issues() {
        let parent = temporary();
        let mut osm = fixture();
        osm.ways.insert(
            10,
            Way {
                id: 10,
                refs: vec![2, 9],
                tags: Tags::from([("highway".into(), "residential".into())]),
            },
        );
        osm.relations.push(Relation {
            id: 5652903,
            tags: Tags::from([
                ("type".into(), "restriction".into()),
                ("restriction".into(), "no_right_turn".into()),
            ]),
            members: vec![
                Member {
                    id: 10,
                    kind: 1,
                    role: String::new(),
                },
                Member {
                    id: 7,
                    kind: 1,
                    role: String::new(),
                },
                Member {
                    id: 99999,
                    kind: 1,
                    role: String::new(),
                },
            ],
        });
        let staged = source(&parent, &osm);
        let output = parent.join("inventory.jsonl");
        let summary =
            audit_staged_restrictions(&staged, &output, &parent.join("fixtures"), 8 * 1024 * 1024)
                .unwrap();
        assert!(summary.complete);
        assert_eq!(summary.restrictions, 2);
        assert_eq!(summary.classifications["MissingFromTo"], 1);
        let rows: Vec<serde_json::Value> = fs::read_to_string(output)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let r = &rows[1];
        for issue in [
            "MissingFromTo",
            "MissingVia",
            "EmptyMemberRole",
            "MissingSourceWay",
            "MissingSourceWayNode",
        ] {
            assert!(
                r["issues"]
                    .as_array()
                    .unwrap()
                    .contains(&serde_json::json!(issue)),
                "{r}"
            );
        }
        assert_eq!(r["way_states"][1]["source_present"], true);
        assert_eq!(r["way_states"][1]["profile_selected"], false);
        assert_eq!(r["way_states"][2]["source_present"], false);
        assert!(parent.join("fixtures/5652903.json").exists());
        assert!(audit_staged_restrictions(
            &staged,
            &parent.join("limited"),
            &parent.join("limited-fixtures"),
            1024
        )
        .is_err());
        fs::remove_dir_all(parent).unwrap();
    }
}
