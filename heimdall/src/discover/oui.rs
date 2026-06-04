//! MAC → manufacturer via the bundled IEEE OUI database (`mac_oui`). Local, no
//! network. Loaded once; lookups are by MAC prefix.

use mac_oui::Oui;

pub struct Vendors {
    db: Oui,
}

impl Vendors {
    /// Load the embedded OUI database (the `with-db` feature). `None` if it
    /// can't be loaded — vendor columns just stay empty.
    pub fn load() -> Option<Self> {
        Oui::default().ok().map(|db| Self { db })
    }

    /// Manufacturer/company name for a MAC, if the OUI is known.
    pub fn lookup(&self, mac: &str) -> Option<String> {
        self.db
            .lookup_by_mac(mac)
            .ok()
            .flatten()
            .map(|e| e.company_name.clone())
    }
}
