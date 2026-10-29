use super::*;
use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};

const MAGIC: &[u8; 8] = b"RWFTML02";
struct Hashed<T> {
    io: T,
    hash: Sha256,
}
impl<T: Read> Read for Hashed<T> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        let n = self.io.read(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
}
impl<T: Write> Write for Hashed<T> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let n = self.io.write(bytes)?;
        self.hash.update(&bytes[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.io.flush()
    }
}
fn io(_: std::io::Error) -> Error {
    Error::Io
}
fn write32(w: &mut impl Write, value: u32) -> Result<(), Error> {
    w.write_all(&value.to_le_bytes()).map_err(io)
}
fn read<const N: usize>(r: &mut impl Read) -> Result<[u8; N], Error> {
    let mut bytes = [0; N];
    r.read_exact(&mut bytes)
        .map_err(|_| Error::InvalidDataset)?;
    Ok(bytes)
}
fn read32(r: &mut impl Read) -> Result<u32, Error> {
    Ok(u32::from_le_bytes(read(r)?))
}
fn read64(r: &mut impl Read) -> Result<u64, Error> {
    Ok(u64::from_le_bytes(read(r)?))
}

impl Index {
    pub(super) fn save(&self, path: &Path, manifest: &Manifest) -> Result<(), Error> {
        if manifest.dataset_id.len() != 64 {
            return Err(Error::InvalidDataset);
        }
        let temp = path.join(format!(
            ".routing-mld-{}-{}.tmp",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| Error::Io)?
                .as_nanos()
        ));
        let file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temp)
            .map_err(io)?;
        let result = (|| {
            let mut writer = BufWriter::new(Hashed {
                io: file,
                hash: Sha256::new(),
            });
            writer.write_all(MAGIC).map_err(io)?;
            writer
                .write_all(manifest.dataset_id.as_bytes())
                .map_err(io)?;
            write32(&mut writer, self.leaf.len() as u32)?;
            write32(&mut writer, self.fanout)?;
            write32(&mut writer, self.levels.len() as u32)?;
            for &cell in &self.leaf {
                write32(&mut writer, cell)?;
            }
            for level in &self.levels {
                write32(&mut writer, level.usable.len() as u32)?;
                write32(&mut writer, level.arcs.len() as u32)?;
                for &usable in &level.usable {
                    writer.write_all(&[u8::from(usable)]).map_err(io)?;
                }
                for &offset in &level.offsets {
                    write32(&mut writer, offset)?;
                }
                for arc in &level.arcs {
                    write32(&mut writer, arc.to)?;
                    writer.write_all(&arc.cost.to_le_bytes()).map_err(io)?;
                }
            }
            let hashed = writer.into_inner().map_err(|_| Error::Io)?;
            let mut file = hashed.io;
            file.write_all(&hashed.hash.finalize()).map_err(io)?;
            file.sync_all().map_err(io)?;
            // Never publish metrics against a manifest replaced during customization.
            let current = routeweft_dataset::inspect_manifest(
                path,
                routeweft_dataset::GraphCapacity::Regional.limits(),
            )
            .map_err(crate::roads::dataset_error)?;
            if current.dataset_id != manifest.dataset_id {
                return Err(Error::InvalidDataset);
            }
            std::fs::rename(&temp, path.join(FILE_NAME)).map_err(io)?;
            File::open(path).and_then(|f| f.sync_all()).map_err(io)
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temp);
        }
        result
    }
    pub(super) fn load(
        path: &Path,
        manifest: &Manifest,
        graph: &Graph,
        limits: LoadOptions,
    ) -> Result<Option<Self>, Error> {
        let path = path.join(FILE_NAME);
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err(Error::Io),
        };
        if !metadata.is_file() || metadata.len() < 116 {
            return Err(Error::InvalidDataset);
        }
        let base = routeweft_dataset::check_resident_budget(manifest, limits, true)
            .map_err(crate::roads::dataset_error)?;
        let remaining = limits.max_resident_bytes.saturating_sub(base);
        // Includes decoded arcs, all level offsets, components, restriction flags,
        // boundary array, reader buffer and transient union-find workspace.
        let budget = SearchBudget {
            max_work: usize::MAX,
            ..Default::default()
        };
        let topology = Topology::build(graph, &mut Context::new(&budget, true), remaining)?;
        let mut charged = topology.states.len() as u64 * 16 + topology.bytes() + 65536;
        let charge = |used: &mut u64, bytes: u64| -> Result<(), Error> {
            *used = used.checked_add(bytes).ok_or(Error::BudgetExceeded)?;
            if *used > remaining {
                return Err(Error::BudgetExceeded);
            }
            Ok(())
        };
        charge(&mut charged, 0)?;
        if metadata.len() > remaining {
            return Err(Error::BudgetExceeded);
        }
        let file = File::open(&path).map_err(io)?;
        let mut reader = BufReader::new(Hashed {
            io: file.take(metadata.len() - 32),
            hash: Sha256::new(),
        });
        if &read::<8>(&mut reader)? != MAGIC {
            return Err(Error::IncompatibleFormat);
        }
        if read::<64>(&mut reader)?.as_slice() != manifest.dataset_id.as_bytes() {
            return Err(Error::InvalidDataset);
        }
        let n = read32(&mut reader)? as usize;
        let fanout = read32(&mut reader)?;
        let levels = read32(&mut reader)? as usize;
        if n != topology.states.len()
            || !(2..=32).contains(&fanout)
            || !(1..=MAX_LEVELS).contains(&levels)
        {
            return Err(Error::InvalidDataset);
        }
        let mut leaf = Vec::with_capacity(n);
        for _ in 0..n {
            let cell = read32(&mut reader)?;
            if cell as usize >= n {
                return Err(Error::InvalidDataset);
            }
            leaf.push(cell);
        }
        let max_cell = *leaf.iter().max().ok_or(Error::InvalidDataset)?;
        let mut index = Self {
            leaf,
            fanout,
            boundary: vec![0; n],
            components: (0..n as u32).collect(),
            topology,
            reverse: Vec::new(),
            levels: Vec::with_capacity(levels),
        };
        for e in &index.topology.edges {
            let a = root(&mut index.components, e.from);
            let b = root(&mut index.components, e.to);
            index.components[a] = b as u32;
            for level in 0..levels {
                if index.cell(level, e.from) != index.cell(level, e.to) {
                    index.boundary[e.from] = index.boundary[e.from].max(level as u8 + 1);
                    index.boundary[e.to] = index.boundary[e.to].max(level as u8 + 1);
                }
            }
        }
        for node in 0..n {
            index.components[node] = root(&mut index.components, node) as u32;
        }
        for level in 0..levels {
            let cells = read32(&mut reader)? as usize;
            let arcs = read32(&mut reader)? as usize;
            if cells != (max_cell / fanout.pow(level as u32)) as usize + 1 {
                return Err(Error::InvalidDataset);
            }
            charge(
                &mut charged,
                cells as u64 + (n as u64 + 1) * 8 + arcs as u64 * 32,
            )?;
            let mut usable = Vec::with_capacity(cells);
            for _ in 0..cells {
                let value = read::<1>(&mut reader)?[0];
                if value > 1 {
                    return Err(Error::InvalidDataset);
                }
                usable.push(value == 1);
            }
            let mut offsets = Vec::with_capacity(n + 1);
            for _ in 0..=n {
                let offset = read32(&mut reader)?;
                if offset as usize > arcs || offsets.last().is_some_and(|&v| v > offset) {
                    return Err(Error::InvalidDataset);
                }
                offsets.push(offset);
            }
            if offsets[0] != 0 || offsets[n] as usize != arcs {
                return Err(Error::InvalidDataset);
            }
            let mut shortcuts = Vec::with_capacity(arcs);
            for _ in 0..arcs {
                let to = read32(&mut reader)?;
                let cost = read64(&mut reader)?;
                if to as usize >= n || cost == 0 {
                    return Err(Error::InvalidDataset);
                }
                shortcuts.push(Shortcut { to, cost });
            }
            for node in 0..n {
                for arc in &shortcuts[offsets[node] as usize..offsets[node + 1] as usize] {
                    let to = arc.to as usize;
                    if node == to
                        || index.boundary[node] as usize <= level
                        || index.boundary[to] as usize <= level
                        || index.cell(level, node) != index.cell(level, to)
                        || !usable[index.cell(level, node)]
                    {
                        return Err(Error::InvalidDataset);
                    }
                }
            }
            index.levels.push(Level {
                offsets,
                arcs: shortcuts,
                usable,
            });
        }
        if reader.read(&mut [0; 1]).map_err(io)? != 0 {
            return Err(Error::InvalidDataset);
        }
        let hashed = reader.into_inner();
        if hashed.io.limit() != 0 {
            return Err(Error::InvalidDataset);
        }
        let mut file = hashed.io.into_inner();
        if hashed.hash.finalize().as_slice() != read::<32>(&mut file)? {
            return Err(Error::InvalidDataset);
        }
        index.rebuild_reverse();
        Ok(Some(index))
    }
}
