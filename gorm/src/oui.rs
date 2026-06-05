//! MAC → manufacturer via the bundled IEEE OUI database (`mac_oui`). Local, no
//! network; loaded once and shared across reads. Mirrors heimdall's helper —
//! kept per-tool rather than shared because the `with-db` feature embeds the
//! whole OUI table into the binary, which only gorm and heimdall want.
//!
//! Bluetooth devices that advertise a random/private address (the
//! locally-administered bit, which no real OUI carries) simply don't resolve —
//! the lookup returns `None` and the vendor column stays empty for them.

use mac_oui::Oui;

pub struct Vendors {
    db: Oui,
}

impl Vendors {
    /// Load the embedded OUI database. `None` if it can't be loaded — gorm just
    /// runs without vendor names.
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
