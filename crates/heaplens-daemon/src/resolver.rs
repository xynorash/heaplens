use std::collections::HashMap;

/// Maps address -> (resolved name, is_machinery). `is_machinery` is supplied
/// by the writer thread (see heaplens-alloc's writer.rs classification) and
/// distinguishes shared allocation-instrumentation frames from genuine
/// caller code, letting the daemon locate a node's real call site without
/// assuming any fixed frame index.
pub struct Resolver {
    map: HashMap<u64, (String, bool)>,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

impl Resolver {
    pub fn new() -> Self {
        Resolver { map: HashMap::new() }
    }

    pub fn insert(&mut self, addr: u64, name: String, is_machinery: bool) {
        self.map.insert(addr, (name, is_machinery));
    }

    /// Returns the resolved name for `addr`, or `"0x{addr:x}"` if unknown.
    pub fn name_for(&self, addr: u64) -> String {
        self.map
            .get(&addr)
            .map(|(name, _)| name.clone())
            .unwrap_or_else(|| format!("0x{addr:x}"))
    }

    /// Returns whether `addr` is classified as shared instrumentation.
    /// An address whose SYMBOLS frame hasn't arrived yet is treated as
    /// machinery (defer, don't guess) — it will stop being skipped as soon
    /// as its real classification arrives, since phi recomputes the
    /// effective site fresh on every match rather than caching it.
    pub fn is_machinery(&self, addr: u64) -> bool {
        self.map.get(&addr).map(|(_, m)| *m).unwrap_or(true)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_addr_returns_name() {
        let mut r = Resolver::new();
        r.insert(0xDEAD_BEEF, "my_func".to_owned(), false);
        assert_eq!(r.name_for(0xDEAD_BEEF), "my_func");
        assert!(!r.is_machinery(0xDEAD_BEEF));
    }

    #[test]
    fn unknown_addr_returns_hex_fallback_and_is_machinery() {
        let r = Resolver::new();
        assert_eq!(r.name_for(0x1234), "0x1234");
        assert_eq!(r.name_for(0), "0x0");
        assert!(r.is_machinery(0x1234));
    }

    #[test]
    fn insert_overwrites_previous() {
        let mut r = Resolver::new();
        r.insert(0x100, "old".to_owned(), true);
        r.insert(0x100, "new".to_owned(), false);
        assert_eq!(r.name_for(0x100), "new");
        assert!(!r.is_machinery(0x100));
    }

    #[test]
    fn machinery_classification_is_queryable_independently_of_name() {
        let mut r = Resolver::new();
        r.insert(0x200, "heaplens_alloc::capture::capture_stack".to_owned(), true);
        r.insert(0x300, "my_crate::do_work".to_owned(), false);
        assert!(r.is_machinery(0x200));
        assert!(!r.is_machinery(0x300));
    }
}
