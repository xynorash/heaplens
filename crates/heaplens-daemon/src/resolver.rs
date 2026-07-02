use std::collections::HashMap;

pub struct Resolver {
    map: HashMap<u64, String>,
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

    pub fn insert(&mut self, addr: u64, name: String) {
        self.map.insert(addr, name);
    }

    /// Returns the resolved name for `addr`, or `"0x{addr:x}"` if unknown.
    pub fn name_for(&self, addr: u64) -> String {
        self.map
            .get(&addr)
            .cloned()
            .unwrap_or_else(|| format!("0x{addr:x}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_addr_returns_name() {
        let mut r = Resolver::new();
        r.insert(0xDEAD_BEEF, "my_func".to_owned());
        assert_eq!(r.name_for(0xDEAD_BEEF), "my_func");
    }

    #[test]
    fn unknown_addr_returns_hex_fallback() {
        let r = Resolver::new();
        assert_eq!(r.name_for(0x1234), "0x1234");
        assert_eq!(r.name_for(0), "0x0");
    }

    #[test]
    fn insert_overwrites_previous() {
        let mut r = Resolver::new();
        r.insert(0x100, "old".to_owned());
        r.insert(0x100, "new".to_owned());
        assert_eq!(r.name_for(0x100), "new");
    }
}
