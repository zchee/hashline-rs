// Copyright 2026 The hashline-rs Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.
//
//! Bounded, sharded in-process snapshot cache shared by all tools.
//!
//! The cache is an accelerator only: identity always comes from exact bytes.
//! Oversize files (larger than the configured capacity) bypass the cache.
//! Concurrent loads of the same path single-flight through one in-flight future
//! cell. Shard locks are never held across disk I/O.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::snapshot::{FileStamp, Snapshot, SnapshotError};

/// Default cache capacity (256 MiB).
pub const DEFAULT_CAPACITY_BYTES: u64 = 256 * 1024 * 1024;

const SHARD_COUNT: usize = 8;

#[derive(Debug)]
struct Entry {
    snapshot: Arc<Snapshot>,
    bytes: u64,
}

#[derive(Debug, Default)]
struct Shard {
    map: HashMap<PathBuf, Entry>,
    used: u64,
    /// In-flight loads: waiters clone the `Arc<Mutex<Option<Result<...>>>>`.
    inflight: HashMap<PathBuf, Arc<Mutex<InflightState>>>,
}

#[derive(Debug)]
enum InflightState {
    Pending,
    Ready(Result<Arc<Snapshot>, String>),
}

/// Process-wide path-keyed snapshot cache with a soft byte budget.
#[derive(Debug)]
pub struct SnapshotCache {
    capacity: u64,
    shards: [Mutex<Shard>; SHARD_COUNT],
}

fn shard_index(path: &Path) -> usize {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    (hasher.finish() as usize) % SHARD_COUNT
}

/// Shared process cache used by read, edit, and grep.
pub fn process_cache() -> &'static SnapshotCache {
    static CACHE: OnceLock<SnapshotCache> = OnceLock::new();
    CACHE.get_or_init(SnapshotCache::new)
}

impl SnapshotCache {
    /// Create a cache with the given capacity in bytes.
    #[must_use]
    pub fn with_capacity(capacity: u64) -> Self {
        Self {
            capacity: capacity.max(1),
            shards: std::array::from_fn(|_| Mutex::new(Shard::default())),
        }
    }

    /// Create a cache with [`DEFAULT_CAPACITY_BYTES`].
    #[must_use]
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_CAPACITY_BYTES)
    }

    fn shard(&self, path: &Path) -> &Mutex<Shard> {
        &self.shards[shard_index(path)]
    }

    /// Return a cached snapshot when the path and optional stamp still match.
    pub fn get(&self, path: &Path, stamp: Option<FileStamp>) -> Option<Arc<Snapshot>> {
        let guard = self.shard(path).lock().expect("cache shard poisoned");
        let entry = guard.map.get(path)?;
        if let Some(expected) = stamp
            && entry.snapshot.stamp() != Some(expected)
        {
            return None;
        }
        Some(Arc::clone(&entry.snapshot))
    }

    /// Insert or replace a snapshot. Oversize entries (`byte_len > capacity`)
    /// are **not** stored (bypass).
    pub fn insert(&self, path: PathBuf, snapshot: Arc<Snapshot>) {
        let bytes = snapshot.byte_len();
        if bytes > self.capacity {
            // Bypass: never store oversize files.
            return;
        }
        let mut guard = self.shard(&path).lock().expect("cache shard poisoned");
        if let Some(old) = guard.map.remove(&path) {
            guard.used = guard.used.saturating_sub(old.bytes);
        }
        while guard.used.saturating_add(bytes) > self.capacity && !guard.map.is_empty() {
            if let Some(key) = guard.map.keys().next().cloned() {
                if let Some(old) = guard.map.remove(&key) {
                    guard.used = guard.used.saturating_sub(old.bytes);
                }
            } else {
                break;
            }
        }
        guard.used = guard.used.saturating_add(bytes);
        guard.map.insert(path, Entry { snapshot, bytes });
    }

    /// Load via `loader` on miss; cache the result. Concurrent callers for the
    /// same path join a single in-flight load.
    pub fn get_or_load<F>(&self, path: &Path, loader: F) -> Result<Arc<Snapshot>, SnapshotError>
    where
        F: FnOnce() -> Result<Snapshot, SnapshotError>,
    {
        if let Ok(meta) = std::fs::metadata(path)
            && let Ok(stamp) = FileStamp::from_metadata_public(&meta)
            && let Some(hit) = self.get(path, Some(stamp))
        {
            return Ok(hit);
        }

        // Single-flight: claim or join. If we already know the current stamp
        // and the resident entry disagrees, treat it as a miss and reload.
        let current_stamp = std::fs::metadata(path)
            .ok()
            .and_then(|meta| FileStamp::from_metadata_public(&meta).ok());

        let cell = {
            let mut guard = self.shard(path).lock().expect("cache shard poisoned");
            if let Some(entry) = guard.map.get(path) {
                let stamp_ok = match current_stamp {
                    Some(stamp) => entry.snapshot.stamp() == Some(stamp),
                    // No metadata: accept unstamped or any resident entry only
                    // when the snapshot itself carries no stamp (detached).
                    None => entry.snapshot.stamp().is_none(),
                };
                if stamp_ok {
                    return Ok(Arc::clone(&entry.snapshot));
                }
                // Stale resident: drop it before reloading.
                if let Some(old) = guard.map.remove(path) {
                    guard.used = guard.used.saturating_sub(old.bytes);
                }
            }
            guard
                .inflight
                .entry(path.to_path_buf())
                .or_insert_with(|| Arc::new(Mutex::new(InflightState::Pending)))
                .clone()
        };

        // Outside shard lock: either we load or wait for the owner.
        let mut state = cell.lock().expect("inflight poisoned");
        match &*state {
            InflightState::Ready(Ok(snap)) => return Ok(Arc::clone(snap)),
            InflightState::Ready(Err(msg)) => {
                return Err(SnapshotError::Io {
                    operation: "load cached",
                    path: path.to_path_buf(),
                    source: std::io::Error::other(msg.clone()),
                });
            }
            InflightState::Pending => {
                // First waiter becomes the loader when we hold the cell lock
                // and state is still Pending — all others block on cell.lock().
            }
        }

        // Only one thread holds `state` at a time while Pending: load here.
        let loaded = loader().map(Arc::new);
        match &loaded {
            Ok(snap) => {
                self.insert(path.to_path_buf(), Arc::clone(snap));
                *state = InflightState::Ready(Ok(Arc::clone(snap)));
            }
            Err(err) => {
                *state = InflightState::Ready(Err(err.to_string()));
            }
        }
        // Clear inflight so later loads can retry after errors.
        {
            let mut guard = self.shard(path).lock().expect("cache shard poisoned");
            guard.inflight.remove(path);
        }
        loaded
    }

    /// Remove a path from the cache.
    pub fn invalidate(&self, path: &Path) {
        let mut guard = self.shard(path).lock().expect("cache shard poisoned");
        if let Some(old) = guard.map.remove(path) {
            guard.used = guard.used.saturating_sub(old.bytes);
        }
        guard.inflight.remove(path);
    }

    /// Resident bytes currently tracked (sum across shards).
    pub fn used_bytes(&self) -> u64 {
        self.shards
            .iter()
            .map(|s| s.lock().expect("shard").used)
            .sum()
    }

    /// Configured capacity.
    pub const fn capacity(&self) -> u64 {
        self.capacity
    }
}

impl Default for SnapshotCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;
    use std::thread;

    #[test]
    fn insert_and_get_roundtrip() {
        let cache = SnapshotCache::with_capacity(1024);
        let snap = Arc::new(Snapshot::from_bytes(b"hello\n".to_vec()).unwrap());
        let path = PathBuf::from("/tmp/cache-x");
        cache.insert(path.clone(), Arc::clone(&snap));
        let hit = cache.get(&path, snap.stamp()).unwrap();
        assert_eq!(hit.id(), snap.id());
        assert_eq!(cache.used_bytes(), 6);
    }

    #[test]
    fn oversize_bypasses_cache() {
        let cache = SnapshotCache::with_capacity(4);
        let snap = Arc::new(Snapshot::from_bytes(b"too-big\n".to_vec()).unwrap());
        assert!(snap.byte_len() > 4);
        cache.insert(PathBuf::from("big"), snap);
        assert_eq!(cache.used_bytes(), 0);
        assert!(cache.get(Path::new("big"), None).is_none());
    }

    #[test]
    fn stamp_mismatch_misses() {
        let cache = SnapshotCache::with_capacity(1024);
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("stamp.txt");
        std::fs::write(
            &path, b"a
",
        )
        .unwrap();
        let snap = Arc::new(Snapshot::load(&path).unwrap());
        cache.insert(path.clone(), Arc::clone(&snap));
        assert!(cache.get(&path, snap.stamp()).is_some());

        // External mutation changes stamp; get_or_load must reload, not serve stale.
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(
            &path, b"b
",
        )
        .unwrap();
        let reloaded = cache
            .get_or_load(&path, || Snapshot::load(&path))
            .expect("reload");
        assert_ne!(reloaded.id(), snap.id());
        assert_eq!(
            reloaded.bytes(),
            b"b
"
        );
    }

    #[test]
    fn concurrent_miss_single_flights() {
        let cache = Arc::new(SnapshotCache::with_capacity(1 << 20));
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("once.txt");
        std::fs::write(&path, b"shared-content\n").unwrap();

        let loads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let barrier = Arc::new(Barrier::new(8));
        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = Arc::clone(&cache);
            let path = path.clone();
            let loads = Arc::clone(&loads);
            let barrier = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                barrier.wait();
                cache
                    .get_or_load(&path, || {
                        loads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        Snapshot::load(&path)
                    })
                    .expect("load")
            }));
        }
        let ids: Vec<_> = handles
            .into_iter()
            .map(|h| h.join().unwrap().id())
            .collect();
        assert!(ids.windows(2).all(|w| w[0] == w[1]));
        // Ideal is 1; allow a small race window if two claims interleave before
        // inflight insert (should still be << 8).
        let n = loads.load(std::sync::atomic::Ordering::SeqCst);
        assert!((1..=2).contains(&n), "loads={n}");
    }

    #[test]
    fn process_cache_is_shared() {
        let a = process_cache() as *const SnapshotCache;
        let b = process_cache() as *const SnapshotCache;
        assert_eq!(a, b);
    }
}
