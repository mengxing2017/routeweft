//! Bounded v2 publication from repeatable, immutable record visitors.
use super::*;

/// Repeatable record source. Visitors must preserve record order between calls.
/// `verify` must authenticate all source files and is called before and after writing.
/// Implementations must bound their own record buffers; these are outside writer budgets.
pub trait BinarySource {
    fn nodes(&self, visitor: &mut dyn FnMut(Point) -> Result<()>) -> Result<()>;
    fn edges(&self, visitor: &mut dyn FnMut(Edge) -> Result<()>) -> Result<()>;
    fn restrictions(&self, visitor: &mut dyn FnMut(&[usize]) -> Result<()>) -> Result<()>;
    fn verify(&self) -> Result<()>;
}

/// Counts and provenance of an immutable source; counts are checked against visits.
pub struct BinaryMetadata {
    pub nodes: usize,
    pub edges: usize,
    pub restrictions: usize,
    pub source_sha256: String,
    pub source_snapshot: Option<i64>,
    pub diagnostics: BTreeMap<String, u64>,
}

/// Logical writer/validator allowances, not process RSS guarantees.
#[derive(Clone, Copy)]
pub struct BinaryWriteOptions {
    /// Owned dictionary entries, conservatively charged before insertion.
    pub dictionary_bytes: u64,
    /// Workspace passed to the independent persisted v2 validator.
    pub validation_bytes: u64,
    /// Total owned graph + manifest bytes. Excludes immutable source files.
    pub output_bytes: u64,
}
impl Default for BinaryWriteOptions {
    fn default() -> Self {
        Self {
            dictionary_bytes: 64 * 1024 * 1024,
            validation_bytes: 64 * 1024 * 1024,
            output_bytes: 257 * 1024 * 1024,
        }
    }
}
struct Staged {
    path: std::path::PathBuf,
}
impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

/// Write deterministic v2 bytes, fully validate persisted semantics and integrity,
/// authenticate the source again, then publish without overwriting an existing path.
/// Source and destination parent must remain immutable to other writers throughout.
/// Controlled failures clean owned staging; crash recovery is not provided.
pub fn write_binary_streaming(
    path: &Path,
    source: &impl BinarySource,
    metadata: BinaryMetadata,
    options: BinaryWriteOptions,
) -> Result<Manifest> {
    if fs::symlink_metadata(path).is_ok() {
        return Err(invalid("output already exists"));
    }
    let limits = LoadOptions::default();
    for (resource, observed, limit) in [
        ("graph_nodes", metadata.nodes, limits.max_nodes),
        ("graph_edges", metadata.edges, limits.max_edges),
        (
            "restriction_sequences",
            metadata.restrictions,
            limits.max_restriction_edges / 2,
        ),
    ] {
        if observed > limit {
            return Err(resource_limit(
                resource,
                limit as u64,
                observed as u64,
                "dataset_write",
            ));
        }
    }
    if metadata.nodes == 0 {
        return Err(invalid("empty graph"));
    }
    source.verify()?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| invalid("clock"))?
        .as_nanos();
    let stage_path = parent.join(format!(".routeweft-stream-{}-{nonce}", std::process::id()));
    fs::create_dir(&stage_path)?;
    let stage = Staged { path: stage_path };
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(stage.path.join("graph.bin"))?;
    let mut writer = BufWriter::new(DigestWriter {
        inner: file,
        digest: Sha256::new(),
        bytes: 0,
        limit: limits.max_bytes.min(options.output_bytes),
        exceeded: None,
    });
    let serialized = binary::stream_write::write(&mut writer, source, &metadata, options);
    let flushed = writer.flush();
    if let Some(observed) = writer.get_ref().exceeded {
        return Err(resource_limit(
            "graph_bytes",
            writer.get_ref().limit,
            observed,
            "dataset_write",
        ));
    }
    serialized?;
    flushed?;
    writer.get_ref().inner.sync_all()?;
    let writer = writer.into_inner().map_err(|e| Error::Io(e.into_error()))?;
    let mut manifest = Manifest {
        format: 2,
        dataset_id: String::new(),
        builder: env!("CARGO_PKG_VERSION").into(),
        profile: "driving-v1".into(),
        source_sha256: metadata.source_sha256,
        source_snapshot: metadata.source_snapshot,
        attribution:
            "© OpenStreetMap contributors; ODbL 1.0; https://www.openstreetmap.org/copyright".into(),
        graph_file: "graph.bin".into(),
        graph_sha256: format!("{:x}", writer.digest.finalize()),
        graph_bytes: writer.bytes,
        nodes: metadata.nodes,
        edges: metadata.edges,
        restrictions: metadata.restrictions,
        diagnostics: metadata.diagnostics,
    };
    manifest.dataset_id = identity(&manifest);
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(stage.path.join("manifest.json"))?;
    let mut output = BufWriter::new(DigestWriter {
        inner: file,
        digest: Sha256::new(),
        bytes: 0,
        limit: (1024 * 1024).min(options.output_bytes.saturating_sub(manifest.graph_bytes)),
        exceeded: None,
    });
    let encoded = serde_json::to_writer_pretty(&mut output, &manifest).map_err(json_stream_error);
    let flushed = output.flush();
    if let Some(observed) = output.get_ref().exceeded {
        return Err(resource_limit(
            "publication_metadata_bytes",
            output.get_ref().limit,
            observed,
            "dataset_write",
        ));
    }
    encoded?;
    flushed?;
    output.get_ref().inner.sync_all()?;
    if validate_binary_streaming(&stage.path, limits, options.validation_bytes)? != manifest {
        return Err(invalid("persisted manifest integrity"));
    }
    source.verify()?;
    File::open(&stage.path)?.sync_all()?;
    // Atomic create reserves the target and rejects even an existing empty directory
    // or dangling symlink; only our own empty reservation can be replaced.
    fs::create_dir(path)?;
    if let Err(error) = fs::rename(&stage.path, path) {
        let _ = fs::remove_dir(path);
        return Err(error.into());
    }
    File::open(parent)?.sync_all()?;
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::path::PathBuf;
    struct Source {
        graph: Graph,
        checks: Cell<usize>,
        fail_final: bool,
        target: Option<PathBuf>,
    }
    impl BinarySource for Source {
        fn nodes(&self, visit: &mut dyn FnMut(Point) -> Result<()>) -> Result<()> {
            for &p in &self.graph.nodes {
                visit(p)?;
            }
            Ok(())
        }
        fn edges(&self, visit: &mut dyn FnMut(Edge) -> Result<()>) -> Result<()> {
            for e in &self.graph.edges {
                visit(e.clone())?;
            }
            Ok(())
        }
        fn restrictions(&self, visit: &mut dyn FnMut(&[usize]) -> Result<()>) -> Result<()> {
            for s in &self.graph.forbidden {
                visit(s)?;
            }
            Ok(())
        }
        fn verify(&self) -> Result<()> {
            self.checks.set(self.checks.get() + 1);
            if self.checks.get() == 2 {
                if self.fail_final {
                    return Err(invalid("source changed"));
                }
                if let Some(path) = &self.target {
                    fs::create_dir(path)?;
                    fs::write(path.join("keep"), "existing")?;
                }
            }
            Ok(())
        }
    }
    fn source() -> Source {
        Source {
            graph: Graph {
                nodes: vec![Point { lon: -0., lat: 0. }],
                edges: vec![
                    Edge {
                        from: 0,
                        to: 0,
                        way: i64::MIN,
                        distance_mm: u64::MAX,
                        duration_ms: 1,
                        name: "道路🚗".into(),
                        class: "residential".into(),
                        roundabout: true,
                    },
                    Edge {
                        from: 0,
                        to: 0,
                        way: i64::MAX,
                        distance_mm: 1,
                        duration_ms: u64::MAX,
                        name: "".into(),
                        class: "service".into(),
                        roundabout: false,
                    },
                ],
                forbidden: vec![vec![1, 0], vec![0, 1]],
            },
            checks: Cell::new(0),
            fail_final: false,
            target: None,
        }
    }
    fn metadata(source: &Source) -> BinaryMetadata {
        BinaryMetadata {
            nodes: source.graph.nodes.len(),
            edges: source.graph.edges.len(),
            restrictions: source.graph.forbidden.len(),
            source_sha256: hash(b"source"),
            source_snapshot: Some(-1),
            diagnostics: BTreeMap::from([("test".into(), 1)]),
        }
    }
    fn temporary() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "routeweft-publish-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn streamed_publication_matches_existing_v2_bytes_manifest_and_loading() {
        let parent = temporary();
        let source = source();
        let a = parent.join("streamed");
        let b = parent.join("resident");
        let m = write_binary_streaming(&a, &source, metadata(&source), Default::default()).unwrap();
        let n = write_with_format(
            &b,
            &source.graph,
            hash(b"source"),
            Some(-1),
            BTreeMap::from([("test".into(), 1)]),
            DatasetFormat::BinaryV2,
        )
        .unwrap();
        assert_eq!(m, n);
        assert_eq!(source.checks.get(), 2);
        for name in ["manifest.json", "graph.bin"] {
            assert_eq!(
                fs::read(a.join(name)).unwrap(),
                fs::read(b.join(name)).unwrap()
            );
        }
        assert_eq!(open(&a, Default::default()).unwrap().1, source.graph);
        assert_eq!(fs::read_dir(&a).unwrap().count(), 2);
        assert!(
            write_binary_streaming(&a, &source, metadata(&source), Default::default()).is_err()
        );
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn all_budgets_and_invalid_persisted_topology_clean_owned_staging() {
        let parent = temporary();
        let target = parent.join("dataset");
        for options in [
            BinaryWriteOptions {
                dictionary_bytes: 0,
                ..Default::default()
            },
            BinaryWriteOptions {
                validation_bytes: 0,
                ..Default::default()
            },
            BinaryWriteOptions {
                output_bytes: 1,
                ..Default::default()
            },
            BinaryWriteOptions {
                output_bytes: 400,
                ..Default::default()
            },
        ] {
            let source = source();
            assert!(matches!(
                write_binary_streaming(&target, &source, metadata(&source), options),
                Err(Error::ResourceLimit(_))
            ));
            assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        }
        for duplicate in [false, true] {
            let mut source = source();
            if duplicate {
                source.graph.forbidden[1] = source.graph.forbidden[0].clone();
            } else {
                source.graph.edges[0].duration_ms = 0;
            }
            assert!(write_binary_streaming(
                &target,
                &source,
                metadata(&source),
                Default::default()
            )
            .is_err());
            assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        }
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn final_source_failure_and_late_existing_target_cannot_publish_or_overwrite() {
        let parent = temporary();
        let target = parent.join("dataset");
        let mut source = source();
        source.fail_final = true;
        assert!(
            write_binary_streaming(&target, &source, metadata(&source), Default::default())
                .is_err()
        );
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        source.fail_final = false;
        source.checks.set(0);
        source.target = Some(target.clone());
        assert!(
            write_binary_streaming(&target, &source, metadata(&source), Default::default())
                .is_err()
        );
        assert_eq!(fs::read(target.join("keep")).unwrap(), b"existing");
        assert_eq!(fs::read_dir(&parent).unwrap().count(), 1);
        fs::remove_dir_all(parent).unwrap();
    }
    #[test]
    fn counts_source_identity_and_metadata_budget_are_checked() {
        let parent = temporary();
        let target = parent.join("dataset");
        let source = source();
        for kind in 0..5 {
            let mut m = metadata(&source);
            match kind {
                0 => m.nodes += 1,
                1 => m.edges -= 1,
                2 => m.restrictions += 1,
                3 => m.source_sha256 = "invalid".into(),
                _ => {
                    m.diagnostics.insert("x".repeat(1024 * 1024), 1);
                }
            }
            assert!(write_binary_streaming(&target, &source, m, Default::default()).is_err());
            assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
        }
        fs::remove_dir_all(parent).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn empty_directory_and_dangling_symlink_are_never_overwritten() {
        let parent = temporary();
        let target = parent.join("dataset");
        let source = source();
        fs::create_dir(&target).unwrap();
        assert!(
            write_binary_streaming(&target, &source, metadata(&source), Default::default())
                .is_err()
        );
        assert!(target.is_dir());
        fs::remove_dir(&target).unwrap();
        std::os::unix::fs::symlink(parent.join("absent"), &target).unwrap();
        assert!(
            write_binary_streaming(&target, &source, metadata(&source), Default::default())
                .is_err()
        );
        assert!(fs::symlink_metadata(&target)
            .unwrap()
            .file_type()
            .is_symlink());
        fs::remove_dir_all(parent).unwrap();
    }
}
