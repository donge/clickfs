//! Inode allocation & path lookup table.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use ahash::RandomState;
use dashmap::DashMap;

pub const INO_ROOT: u64 = 1;

pub struct InodeTable {
    forward: DashMap<u64, PathBuf>,
    reverse: DashMap<PathBuf, u64>,
    hasher: RandomState,
    /// Counter for collision resolution.
    bump: AtomicU64,
}

impl InodeTable {
    pub fn new() -> Self {
        let table = Self {
            forward: DashMap::new(),
            reverse: DashMap::new(),
            hasher: RandomState::with_seeds(0x4f43_4646, 0xdead_beef, 0xc01d_b00b, 0x1337_c0de),
            bump: AtomicU64::new(0),
        };
        // Pin the root.
        table.forward.insert(INO_ROOT, PathBuf::from("/"));
        table.reverse.insert(PathBuf::from("/"), INO_ROOT);
        table
    }

    pub fn allocate(&self, path: &Path) -> u64 {
        if let Some(existing) = self.reverse.get(path) {
            return *existing;
        }
        // Reserve low values.
        let mut ino = std::cmp::max(self.hash(path), 100);
        loop {
            // Avoid clobbering the root.
            if ino == INO_ROOT {
                ino = ino.wrapping_add(self.bump.fetch_add(1, Ordering::Relaxed) + 1);
                continue;
            }
            // Race-free insertion via DashMap entry API.
            let entry = self.forward.entry(ino);
            match entry {
                dashmap::mapref::entry::Entry::Occupied(occ) => {
                    if occ.get() == path {
                        // Some other thread inserted the same path → keep theirs.
                        let i = *occ.key();
                        self.reverse.insert(path.to_path_buf(), i);
                        return i;
                    } else {
                        // Collision with a different path → bump.
                        ino = ino.wrapping_add(1);
                        continue;
                    }
                }
                dashmap::mapref::entry::Entry::Vacant(vac) => {
                    vac.insert(path.to_path_buf());
                    self.reverse.insert(path.to_path_buf(), ino);
                    return ino;
                }
            }
        }
    }

    pub fn lookup(&self, ino: u64) -> Option<PathBuf> {
        self.forward.get(&ino).map(|r| r.clone())
    }

    fn hash(&self, path: &Path) -> u64 {
        use std::hash::{BuildHasher, Hash, Hasher};
        let mut h = self.hasher.build_hasher();
        path.hash(&mut h);
        let v = h.finish();
        // Make sure we never return 0 or 1 (1 is root).
        v | 0x2
    }
}

impl Default for InodeTable {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_pinned() {
        let t = InodeTable::new();
        assert_eq!(t.lookup(INO_ROOT).unwrap(), PathBuf::from("/"));
    }

    #[test]
    fn allocate_stable() {
        let t = InodeTable::new();
        let a = t.allocate(Path::new("/db/foo"));
        let b = t.allocate(Path::new("/db/foo"));
        assert_eq!(a, b);
        assert!(a > 1);
    }

    #[test]
    fn distinct_paths() {
        let t = InodeTable::new();
        let a = t.allocate(Path::new("/db/foo"));
        let b = t.allocate(Path::new("/db/bar"));
        assert_ne!(a, b);
    }
}
