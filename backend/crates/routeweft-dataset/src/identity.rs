//! Expected immutable dataset identity, independent of host storage and authorization.
use crate::{invalid, Manifest, Result};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DatasetIdentity {
    pub dataset_id: String,
    pub format: u32,
    pub profile: String,
    pub source_sha256: String,
    pub attribution: String,
}
impl DatasetIdentity {
    pub fn validate(&self) -> Result<()> {
        if !matches!(self.format, 1 | 2) {
            return Err(crate::Error::IncompatibleFormat);
        }
        if self.profile != "driving-v1" {
            return Err(invalid("unsupported profile"));
        }
        if !is_digest(&self.dataset_id)
            || !is_digest(&self.source_sha256)
            || self.attribution.trim().is_empty()
        {
            return Err(invalid("invalid identity"));
        }
        Ok(())
    }
    pub fn verify(&self, manifest: &Manifest) -> Result<()> {
        self.validate()?;
        if self != &Self::from(manifest) {
            return Err(invalid("dataset identity mismatch"));
        }
        Ok(())
    }
}
impl From<&Manifest> for DatasetIdentity {
    fn from(m: &Manifest) -> Self {
        Self {
            dataset_id: m.dataset_id.clone(),
            format: m.format,
            profile: m.profile.clone(),
            source_sha256: m.source_sha256.clone(),
            attribution: m.attribution.clone(),
        }
    }
}
pub fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_identity_matches_both_formats_and_rejects_each_changed_field() {
        for format in [1, 2] {
            let manifest = Manifest {
                format,
                dataset_id: "a".repeat(64),
                builder: "fixture".into(),
                profile: "driving-v1".into(),
                source_sha256: "b".repeat(64),
                source_snapshot: None,
                attribution: "Synthetic".into(),
                graph_file: if format == 1 {
                    "graph.json"
                } else {
                    "graph.bin"
                }
                .into(),
                graph_sha256: "c".repeat(64),
                graph_bytes: 1,
                nodes: 2,
                edges: 1,
                restrictions: 0,
                diagnostics: Default::default(),
            };
            let expected = DatasetIdentity::from(&manifest);
            expected.verify(&manifest).unwrap();
            for field in 0..5 {
                let mut wrong = expected.clone();
                match field {
                    0 => wrong.dataset_id = "d".repeat(64),
                    1 => wrong.format = 3 - format,
                    2 => wrong.profile = "walking-v1".into(),
                    3 => wrong.source_sha256 = "e".repeat(64),
                    _ => wrong.attribution = "Other".into(),
                }
                assert!(wrong.verify(&manifest).is_err(), "field {field}");
            }
            let mut wrong = expected;
            wrong.format = 3;
            assert!(matches!(
                wrong.validate(),
                Err(crate::Error::IncompatibleFormat)
            ));
            wrong.format = format;
            wrong.dataset_id = "G".repeat(64);
            assert!(wrong.validate().is_err());
        }
    }
}
