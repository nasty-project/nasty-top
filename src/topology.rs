//! Resolve kernel block-layer backing devices without adding their I/O to the
//! filesystem member's counters. In particular dm/partition layers are not SSDs
//! just because their own queue reports non-rotational storage.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaKind {
    Rotational,
    Nvme,
    NonRotational,
}

impl MediaKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Rotational => "rotational",
            Self::Nvme => "NVMe",
            Self::NonRotational => "non-rotational",
        }
    }
    pub fn latency_floor_ms(self) -> f64 {
        match self {
            Self::Rotational => 20.0,
            Self::Nvme => 2.0,
            Self::NonRotational => 5.0,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BlockTopology {
    pub leaves: Vec<String>,
    /// None means unknown or mixed media; no partial graph is used for peers.
    pub media: Option<MediaKind>,
}

type Leaves = BTreeMap<String, Option<MediaKind>>;

pub struct Resolver {
    root: PathBuf,
    cache: HashMap<String, Leaves>,
}

impl Resolver {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.into(),
            cache: HashMap::new(),
        }
    }

    pub fn resolve(&mut self, name: &str) -> BlockTopology {
        let Some(leaves) = self.walk(name, &mut HashSet::new(), 0) else {
            return BlockTopology::default();
        };
        let media = leaves
            .values()
            .next()
            .copied()
            .flatten()
            .filter(|kind| leaves.values().all(|other| *other == Some(*kind)));
        BlockTopology {
            leaves: leaves.into_keys().collect(),
            media,
        }
    }

    fn walk(&mut self, name: &str, visiting: &mut HashSet<String>, depth: usize) -> Option<Leaves> {
        if name.is_empty()
            || name.contains('/')
            || matches!(name, "." | "..")
            || depth > 16
            || self.cache.len() + visiting.len() >= 4096
            || !visiting.insert(name.into())
        {
            return None;
        }
        let result = if let Some(cached) = self.cache.get(name) {
            Some(cached.clone())
        } else {
            self.read_node(name, visiting, depth)
        };
        visiting.remove(name);
        // Depth/cycle failures depend on the traversal path. Do not poison a
        // later shallow lookup of the same node by caching such a failure.
        if let Some(leaves) = &result {
            self.cache.insert(name.into(), leaves.clone());
        }
        result
    }

    fn read_node(
        &mut self,
        name: &str,
        visiting: &mut HashSet<String>,
        depth: usize,
    ) -> Option<Leaves> {
        let node = self.root.join(name);
        let canonical = std::fs::canonicalize(&node).ok()?;
        if node.join("partition").exists() {
            let parent = canonical.parent()?.file_name()?.to_str()?;
            return self.walk(parent, visiting, depth + 1);
        }
        let mut slaves = std::fs::read_dir(node.join("slaves"))
            .ok()?
            .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
            .collect::<Result<Vec<_>, _>>()
            .ok()?;
        slaves.sort();
        if slaves.is_empty() {
            if name.starts_with("dm-") || name.starts_with("md") {
                return None;
            }
            let rotational = std::fs::read_to_string(node.join("queue/rotational")).ok();
            let media = match rotational.as_deref().map(str::trim) {
                Some("1") => Some(MediaKind::Rotational),
                Some("0") if name.starts_with("nvme") => Some(MediaKind::Nvme),
                Some("0") => Some(MediaKind::NonRotational),
                _ => None,
            };
            return Some(BTreeMap::from([(name.into(), media)]));
        }
        let mut leaves = BTreeMap::new();
        for slave in slaves {
            leaves.extend(self.walk(&slave, visiting, depth + 1)?);
            if leaves.len() > 256 {
                return None;
            }
        }
        Some(leaves)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "nasty-top-topology-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(root.join("class")).unwrap();
            Self(root)
        }
        fn disk(&self, name: &str, rotational: &str) {
            let path = self.0.join("devices").join(name);
            std::fs::create_dir_all(path.join("slaves")).unwrap();
            std::fs::create_dir_all(path.join("queue")).unwrap();
            std::fs::write(path.join("queue/rotational"), rotational).unwrap();
            symlink(
                format!("../devices/{name}"),
                self.0.join("class").join(name),
            )
            .unwrap();
        }
        fn mapper(&self, name: &str, slaves: &[&str]) {
            self.disk(name, "0");
            for slave in slaves {
                symlink(
                    format!("../../../class/{slave}"),
                    self.0.join(format!("devices/{name}/slaves/{slave}")),
                )
                .unwrap();
            }
        }
        fn resolve(&self, name: &str) -> BlockTopology {
            Resolver::new(&self.0.join("class")).resolve(name)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn nested_dm_and_partitions_resolve_to_the_rotational_backing_disk() {
        let f = Fixture::new();
        f.disk("sda", "1");
        std::fs::create_dir_all(f.0.join("devices/sda/sda1")).unwrap();
        std::fs::write(f.0.join("devices/sda/sda1/partition"), "1").unwrap();
        symlink("../devices/sda/sda1", f.0.join("class/sda1")).unwrap();
        f.mapper("dm-0", &["sda1"]);
        f.mapper("dm-1", &["dm-0"]);
        let topology = f.resolve("dm-1");
        assert_eq!(topology.media, Some(MediaKind::Rotational));
        assert_eq!(topology.leaves, ["sda"]);
    }

    #[test]
    fn mixed_media_missing_slaves_and_cycles_are_unknown_not_ssds() {
        let f = Fixture::new();
        f.disk("sda", "1");
        f.disk("nvme0n1", "0");
        f.mapper("dm-0", &["sda", "nvme0n1"]);
        assert_eq!(f.resolve("dm-0").media, None);
        assert_eq!(f.resolve("dm-0").leaves.len(), 2);
        f.mapper("dm-1", &["missing"]);
        assert_eq!(f.resolve("dm-1"), BlockTopology::default());
        f.mapper("dm-2", &["dm-3"]);
        f.mapper("dm-3", &["dm-2"]);
        assert_eq!(f.resolve("dm-2"), BlockTopology::default());
        f.mapper("dm-4", &[]);
        assert_eq!(f.resolve("dm-4").media, None);
        assert_eq!(f.resolve("nvme0n1").media, Some(MediaKind::Nvme));
    }

    #[test]
    fn depth_limit_does_not_poison_a_later_shallow_lookup() {
        let f = Fixture::new();
        f.disk("sda", "1");
        f.mapper("dm-0", &["sda"]);
        for i in 1..=18 {
            f.mapper(&format!("dm-{i}"), &[&format!("dm-{}", i - 1)]);
        }
        let mut resolver = Resolver::new(&f.0.join("class"));
        assert_eq!(resolver.resolve("dm-18"), BlockTopology::default());
        assert_eq!(resolver.resolve("dm-2").media, Some(MediaKind::Rotational));
    }
}
