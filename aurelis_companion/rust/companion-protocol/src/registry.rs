//! Bounded registry of in-flight pairings, keyed by an opaque id (never a raw pointer).
//! Capped at [`CAP`] entries (oldest evicted) and entries older than [`MAX_AGE_MS`] (the code's
//! lifetime) are dropped on every insert. Time is injected so it tests without a clock.

use crate::pairing::Pairing;
use std::collections::HashMap;

pub const CAP: usize = 8;
pub const MAX_AGE_MS: u64 = 10 * 60 * 1000;

#[derive(Default)]
pub struct Registry {
    next: u64,
    live: HashMap<u64, (Pairing, u64)>,
}

impl Registry {
    pub fn new() -> Self {
        Registry { next: 1, live: HashMap::new() }
    }

    pub fn len(&self) -> usize {
        self.live.len()
    }

    pub fn is_empty(&self) -> bool {
        self.live.is_empty()
    }

    pub fn insert(&mut self, p: Pairing, now_ms: u64) -> u64 {
        self.live.retain(|_, (_, at)| now_ms.saturating_sub(*at) < MAX_AGE_MS);
        while self.live.len() >= CAP {
            let oldest = self.live.iter().min_by_key(|(id, (_, at))| (*at, **id)).map(|(id, _)| *id);
            match oldest {
                Some(id) => {
                    self.live.remove(&id);
                }
                None => break,
            }
        }
        if self.next == 0 {
            self.next = 1;
        }
        let id = self.next;
        self.next += 1;
        self.live.insert(id, (p, now_ms));
        id
    }

    /// Removes and returns the pairing; None when unknown, already taken, or older than its life.
    pub fn take(&mut self, id: u64, now_ms: u64) -> Option<Pairing> {
        let (p, at) = self.live.remove(&id)?;
        (now_ms.saturating_sub(at) < MAX_AGE_MS).then_some(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p() -> Pairing {
        Pairing::start("7KQ2M9XA").0
    }

    #[test]
    fn take_is_once_only() {
        let mut r = Registry::new();
        let id = r.insert(p(), 0);
        assert!(r.take(id, 1).is_some());
        assert!(r.take(id, 1).is_none());
    }

    #[test]
    fn unknown_handle_is_none() {
        let mut r = Registry::new();
        assert!(r.take(999, 0).is_none());
        assert!(r.take(0, 0).is_none());
    }

    #[test]
    fn double_finish_is_none() {
        let mut r = Registry::new();
        let (a, _) = Pairing::start("7KQ2M9XA");
        let (_, bm) = Pairing::start("7KQ2M9XA");
        let id = r.insert(a, 0);
        let first = r.take(id, 5).unwrap().finish(&bm);
        assert!(first.is_some());
        assert!(r.take(id, 5).is_none());
    }

    #[test]
    fn cap_evicts_the_oldest() {
        let mut r = Registry::new();
        let ids: Vec<u64> = (0..CAP as u64 + 3).map(|i| r.insert(p(), i)).collect();
        assert_eq!(r.len(), CAP);
        for old in &ids[..3] {
            assert!(r.take(*old, 100).is_none());
        }
        assert!(r.take(*ids.last().unwrap(), 100).is_some());
    }

    #[test]
    fn old_entries_are_dropped_on_insert() {
        let mut r = Registry::new();
        let old = r.insert(p(), 0);
        let young = r.insert(p(), MAX_AGE_MS - 1);
        let _new = r.insert(p(), MAX_AGE_MS + 1);
        assert_eq!(r.len(), 2);
        assert!(r.take(old, MAX_AGE_MS + 1).is_none());
        assert!(r.take(young, MAX_AGE_MS + 1).is_some());
    }

    #[test]
    fn expired_entry_cannot_be_taken() {
        let mut r = Registry::new();
        let id = r.insert(p(), 0);
        assert!(r.take(id, MAX_AGE_MS).is_none());
    }
}
