//! E5: promoted compositing layers — a small whole-surface raster cache for
//! the C-ABI present loop.
//!
//! Layout stamps `will-change`/`isolate` subtrees with group ids (B8), but
//! those ride the wire host-side (frame.rs skips them): per-group layer
//! isolation itself is the Swift host's job. The painter-side win is
//! caching the rendered page layer across presents that carry
//! byte-identical frames — a static page then costs one blit instead of a
//! full parse + re-raster. The key hashes the RAW frame bytes (not the
//! parsed scene: glyphs never reach the C-ABI scene, so a scene hash would
//! miss text-only changes) plus the out-of-frame state that changes pixels:
//! surface size, scale, scroll origin and the LCD flag. Image rasters
//! resolve through the engine side-table with the frame unchanged, so a
//! host that mutates that table (animated images) must clear the cache —
//! `kru_gpu_clear_layers`.

use std::collections::VecDeque;

const FNV_OFFSET: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

/// FNV-1a layer key over the raw frame bytes and the out-of-frame render
/// state (size, scale, scroll, LCD). Callers pass the same clamped scale
/// and non-negative origins they render with.
pub fn layer_key(frame: &[u8], w: u32, h: u32, scale: f32, ox: f32, oy: f32, lcd: bool) -> u64 {
    let mut hash = FNV_OFFSET;
    for &b in frame {
        hash = (hash ^ b as u64).wrapping_mul(FNV_PRIME);
    }
    for v in [
        w as u64,
        h as u64,
        scale.to_bits() as u64,
        ox.to_bits() as u64,
        oy.to_bits() as u64,
        lcd as u64,
    ] {
        for b in v.to_le_bytes() {
            hash = (hash ^ b as u64).wrapping_mul(FNV_PRIME);
        }
    }
    hash
}

/// LRU cache of whole rendered layers, keyed by [`layer_key`]. Small by
/// design — a handful of scroll positions or alternated frames, not a full
/// tile grid (that is [`crate::tiles::TileCache`], which subdivides the
/// surface and does dirty-rect invalidation instead).
pub struct LayerCache {
    cap: usize,
    entries: VecDeque<(u64, Vec<u8>)>,
    pub hits: u64,
    pub misses: u64,
}

impl LayerCache {
    pub fn new() -> LayerCache {
        LayerCache::with_capacity(4)
    }

    pub fn with_capacity(cap: usize) -> LayerCache {
        LayerCache {
            cap,
            entries: VecDeque::new(),
            hits: 0,
            misses: 0,
        }
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Fresh copy of the cached raster on a key match (and a touch — the
    /// entry moves to most-recent); `None` on a miss (counted).
    pub fn lookup(&mut self, key: u64) -> Option<Vec<u8>> {
        let Some(i) = self.entries.iter().position(|(k, _)| *k == key) else {
            self.misses += 1;
            return None;
        };
        let entry = self.entries.remove(i).unwrap();
        self.hits += 1;
        let px = entry.1.clone();
        self.entries.push_back(entry);
        Some(px)
    }

    pub fn store(&mut self, key: u64, px: &[u8]) {
        if self.cap == 0 {
            return;
        }
        if let Some(i) = self.entries.iter().position(|(k, _)| *k == key) {
            self.entries.remove(i);
        }
        if self.entries.len() >= self.cap {
            self.entries.pop_front();
        }
        self.entries.push_back((key, px.to_vec()));
    }
}

impl Default for LayerCache {
    fn default() -> Self {
        LayerCache::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_hits_and_misses() {
        let mut c = LayerCache::new();
        assert_eq!(c.lookup(1), None);
        assert_eq!((c.hits, c.misses), (0, 1));
        c.store(1, &[1, 2, 3]);
        assert_eq!(c.lookup(1), Some(vec![1, 2, 3]));
        assert_eq!((c.hits, c.misses), (1, 1));
        assert_eq!(c.len(), 1);
        // The stored copy is independent of later mutation of the source.
        let src = vec![9u8; 8];
        c.store(2, &src);
        assert_eq!(c.lookup(2), Some(vec![9u8; 8]));
    }

    #[test]
    fn lru_evicts_oldest_and_touch_refreshes() {
        let mut c = LayerCache::with_capacity(2);
        c.store(1, &[1]);
        c.store(2, &[2]);
        assert_eq!(c.lookup(1), Some(vec![1]), "touch 1 → 2 becomes oldest");
        c.store(3, &[3]);
        assert_eq!(c.lookup(2), None, "2 was evicted");
        assert_eq!(c.lookup(1), Some(vec![1]));
        assert_eq!(c.lookup(3), Some(vec![3]));
        assert_eq!(c.len(), 2);
    }

    #[test]
    fn store_same_key_replaces_and_clear_drops_all() {
        let mut c = LayerCache::new();
        c.store(5, &[1]);
        c.store(5, &[2, 2]);
        assert_eq!(c.len(), 1);
        assert_eq!(c.lookup(5), Some(vec![2, 2]));
        c.clear();
        assert!(c.is_empty());
        assert_eq!(c.lookup(5), None);
    }

    #[test]
    fn zero_capacity_never_stores() {
        let mut c = LayerCache::with_capacity(0);
        c.store(1, &[1]);
        assert!(c.is_empty());
        assert_eq!(c.lookup(1), None);
    }

    #[test]
    fn key_separates_every_render_input() {
        let f = [1u8, 2, 3];
        let base = layer_key(&f, 10, 10, 1.0, 0.0, 0.0, false);
        assert_eq!(
            layer_key(&f, 10, 10, 1.0, 0.0, 0.0, false),
            base,
            "deterministic"
        );
        assert_ne!(
            layer_key(&[1, 2, 4], 10, 10, 1.0, 0.0, 0.0, false),
            base,
            "frame bytes"
        );
        assert_ne!(layer_key(&f, 11, 10, 1.0, 0.0, 0.0, false), base, "width");
        assert_ne!(layer_key(&f, 10, 11, 1.0, 0.0, 0.0, false), base, "height");
        assert_ne!(layer_key(&f, 10, 10, 2.0, 0.0, 0.0, false), base, "scale");
        assert_ne!(
            layer_key(&f, 10, 10, 1.0, 5.0, 0.0, false),
            base,
            "scroll x"
        );
        assert_ne!(
            layer_key(&f, 10, 10, 1.0, 0.0, 5.0, false),
            base,
            "scroll y"
        );
        assert_ne!(layer_key(&f, 10, 10, 1.0, 0.0, 0.0, true), base, "lcd flag");
        assert_ne!(
            layer_key(&[], 10, 10, 1.0, 0.0, 0.0, false),
            base,
            "empty frame"
        );
    }
}
