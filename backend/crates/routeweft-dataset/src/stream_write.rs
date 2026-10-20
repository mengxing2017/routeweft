//! Writer sharing the v2 constants and primitive encoding with the resident codec.
use super::*;
use crate::{BinaryMetadata, BinarySource, BinaryWriteOptions};
use std::collections::BTreeMap;

pub(crate) fn write(
    writer: &mut impl Write,
    source: &impl BinarySource,
    metadata: &BinaryMetadata,
    options: BinaryWriteOptions,
) -> Result<()> {
    let limits = LoadOptions::default();
    let mut dictionary = BTreeMap::<String, usize>::new();
    let mut charged = 0_u64;
    let mut expanded = 0_u64;
    let mut edges = 0;
    source.edges(&mut |edge| {
        edges += 1;
        if edges > metadata.edges || edge.name.len() > 4096 || edge.class.len() > 64 {
            return Err(invalid("stream edge count/string length"));
        }
        expanded += (edge.name.len() + edge.class.len()) as u64;
        if expanded > MAX_STRING_BYTES {
            return Err(resource_limit(
                "graph_string_bytes",
                MAX_STRING_BYTES,
                expanded,
                "dataset_write",
            ));
        }
        for value in [edge.name, edge.class] {
            if let std::collections::btree_map::Entry::Vacant(entry) = dictionary.entry(value) {
                let observed = charged
                    .checked_add(128 + entry.key().capacity() as u64)
                    .ok_or(Error::LimitExceeded)?;
                if observed > options.dictionary_bytes {
                    return Err(resource_limit(
                        "dictionary_workspace_bytes",
                        options.dictionary_bytes,
                        observed,
                        "dataset_write",
                    ));
                }
                charged = observed;
                entry.insert(0);
            }
        }
        Ok(())
    })?;
    if edges != metadata.edges {
        return Err(invalid("stream edge count"));
    }
    for (id, value) in dictionary.values_mut().enumerate() {
        *value = id;
    }
    let dictionary_bytes = dictionary.keys().map(|value| 4 + value.len() as u64).sum();
    let mut sequences = 0;
    let mut refs = 0_u64;
    source.restrictions(&mut |sequence| {
        sequences += 1;
        refs = refs
            .checked_add(sequence.len() as u64)
            .ok_or(Error::LimitExceeded)?;
        if refs > limits.max_restriction_edges as u64 {
            return Err(resource_limit(
                "restriction_edges",
                limits.max_restriction_edges as u64,
                refs,
                "dataset_write",
            ));
        }
        if sequences > metadata.restrictions
            || !(2..=256).contains(&sequence.len())
            || sequence.iter().any(|&id| id >= edges)
        {
            return Err(invalid("stream restriction"));
        }
        Ok(())
    })?;
    if sequences != metadata.restrictions {
        return Err(invalid("stream restriction count"));
    }
    let size = layout(
        metadata.nodes as u64,
        edges as u64,
        sequences as u64,
        refs,
        dictionary_bytes,
    )?;
    let limit = limits.max_bytes.min(options.output_bytes);
    if size > limit {
        return Err(resource_limit("graph_bytes", limit, size, "dataset_write"));
    }
    writer.write_all(MAGIC)?;
    for value in [
        0,
        metadata.nodes as u64,
        edges as u64,
        sequences as u64,
        dictionary.len() as u64,
        refs,
        dictionary_bytes,
    ] {
        u64_write(writer, value)?;
    }
    for value in dictionary.keys() {
        u32_write(writer, value.len())?;
        writer.write_all(value.as_bytes())?;
    }
    let mut nodes = 0;
    source.nodes(&mut |point| {
        nodes += 1;
        if nodes > metadata.nodes {
            return Err(invalid("stream node count"));
        }
        point.coordinate()?;
        if point.lat.abs() > 85. {
            return Err(invalid("supported latitude is [-85,85]"));
        }
        writer.write_all(&point.lon.to_le_bytes())?;
        writer.write_all(&point.lat.to_le_bytes())?;
        Ok(())
    })?;
    if nodes != metadata.nodes {
        return Err(invalid("stream node count"));
    }
    let mut written = 0;
    source.edges(&mut |edge| {
        written += 1;
        if written > edges {
            return Err(invalid("stream edge count"));
        }
        for value in [
            edge.from as u64,
            edge.to as u64,
            edge.way as u64,
            edge.distance_mm,
            edge.duration_ms,
        ] {
            u64_write(writer, value)?;
        }
        for value in [&edge.name, &edge.class] {
            u32_write(
                writer,
                *dictionary
                    .get(value)
                    .ok_or_else(|| invalid("stream dictionary changed"))?,
            )?;
        }
        u64_write(writer, u64::from(edge.roundabout))?;
        Ok(())
    })?;
    if written != edges {
        return Err(invalid("stream edge count"));
    }
    let mut written = 0;
    let mut written_refs = 0;
    source.restrictions(&mut |sequence| {
        written += 1;
        written_refs += sequence.len() as u64;
        if written > sequences || written_refs > refs || !(2..=256).contains(&sequence.len()) {
            return Err(invalid("stream restriction changed"));
        }
        u32_write(writer, sequence.len())?;
        for &edge in sequence {
            u64_write(writer, edge as u64)?;
        }
        Ok(())
    })?;
    if written != sequences || written_refs != refs {
        return Err(invalid("stream restriction changed"));
    }
    // Owned dictionary is dropped before persisted validation starts.
    Ok(())
}
