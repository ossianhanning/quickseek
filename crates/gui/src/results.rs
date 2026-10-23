//! Client-side view of a result set: total count plus pages of rows fetched
//! on demand as the virtual list asks for them.

use std::collections::HashMap;

use quickseek_core::ipc::{Page, Row};

pub const BLOCK: usize = 256;
/// Blocks kept around; beyond this the farthest ones are dropped.
const MAX_BLOCKS: usize = 64;

#[derive(Default)]
pub struct Results {
    /// Search sequence number these rows belong to.
    pub seq: u64,
    pub total: usize,
    pub elapsed_ms: f64,
    pub generation: u64,
    /// Content search: which filtering step of the result set these rows show.
    pub ver: u64,
    blocks: HashMap<usize, Vec<Row>>,
}

impl Results {
    pub fn new(seq: u64, page: Page) -> Results {
        let mut r = Results {
            seq,
            total: page.total as usize,
            elapsed_ms: page.elapsed_us as f64 / 1000.0,
            generation: page.generation,
            ver: 0,
            blocks: HashMap::new(),
        };
        r.insert(page.offset as usize, page.rows);
        r
    }

    /// Stores rows starting at `offset` (split into blocks; partial trailing
    /// blocks are only kept when they reach the end of the results).
    pub fn insert(&mut self, offset: usize, rows: Vec<Row>) {
        let mut i = 0;
        while i < rows.len() {
            let at = offset + i;
            let b = at / BLOCK;
            let start = b * BLOCK;
            if at != start {
                // Not block aligned: skip to the next block boundary.
                i += BLOCK - (at - start);
                continue;
            }
            let end = (i + BLOCK).min(rows.len());
            let complete = end - i == BLOCK || start + (end - i) >= self.total;
            if complete {
                self.blocks.insert(b, rows[i..end].to_vec());
            }
            i = end;
        }
        self.evict_far_from(offset / BLOCK);
    }

    fn evict_far_from(&mut self, center: usize) {
        if self.blocks.len() <= MAX_BLOCKS {
            return;
        }
        let mut keys: Vec<usize> = self.blocks.keys().copied().collect();
        keys.sort_by_key(|&k| std::cmp::Reverse(k.abs_diff(center)));
        for k in keys.into_iter().take(self.blocks.len() - MAX_BLOCKS) {
            self.blocks.remove(&k);
        }
    }

    pub fn get(&self, i: usize) -> Option<&Row> {
        self.blocks.get(&(i / BLOCK)).and_then(|b| b.get(i % BLOCK))
    }

    /// Block start offsets needed to show rows `from..=to` that aren't loaded.
    pub fn missing(&self, from: usize, to: usize) -> Vec<usize> {
        if self.total == 0 {
            return Vec::new();
        }
        let to = to.min(self.total - 1);
        (from / BLOCK..=to / BLOCK)
            .filter(|b| !self.blocks.contains_key(b))
            .map(|b| b * BLOCK)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(from: usize, n: usize) -> Vec<Row> {
        (from..from + n)
            .map(|i| Row {
                name: format!("r{i}"),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn first_page_and_on_demand_blocks() {
        let mut r = Results::new(
            1,
            Page {
                total: 1000,
                rows: rows(0, 300),
                ..Default::default()
            },
        );
        assert_eq!(r.get(0).unwrap().name, "r0");
        assert_eq!(r.get(255).unwrap().name, "r255");
        // The partial second block (256..300) was not kept.
        assert!(r.get(256).is_none());
        assert_eq!(r.missing(200, 600), vec![256, 512]);
        r.insert(512, rows(512, 256));
        assert_eq!(r.get(700).unwrap().name, "r700");
        assert_eq!(r.missing(200, 600), vec![256]);
        // Last partial block at the end of the results is kept.
        r.insert(768, rows(768, 232));
        assert_eq!(r.get(999).unwrap().name, "r999");
        assert!(r.missing(990, 5000).is_empty());
    }

    #[test]
    fn evicts_far_blocks() {
        let mut r = Results::new(
            1,
            Page {
                total: 1_000_000,
                rows: rows(0, 256),
                ..Default::default()
            },
        );
        for b in 1..200 {
            r.insert(b * BLOCK, rows(b * BLOCK, BLOCK));
        }
        assert!(r.blocks.len() <= MAX_BLOCKS);
        assert!(r.get(199 * BLOCK).is_some());
        assert!(r.get(0).is_none());
    }
}
