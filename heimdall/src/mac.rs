//! MAC address bit semantics. Everything a MAC *means* (beyond the OUI, which we
//! resolve against the IEEE database in `discover::oui`) lives in the two
//! least-significant bits of the **first octet** — IEEE 802 defines only these:
//!
//! - **I/G bit** (bit 0): Individual (0, unicast — one NIC) vs Group (1,
//!   multicast, or the all-ones broadcast). A real discovered host is unicast.
//! - **U/L bit** (bit 1): Universal (0, IEEE-assigned OUI, globally unique) vs
//!   Local (1, *locally administered* — privacy-randomized, virtual, or
//!   hand-set). When this bit is set the OUI is meaningless, which is why a
//!   randomized MAC never resolves to a vendor.
//!
//! Corollary: a locally-administered unicast MAC always has its second hex
//! digit in {2, 6, A, E}.

/// What a MAC's address bits say about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Universally administered unicast — a real, IEEE-assigned hardware MAC.
    /// The OUI is meaningful (this is the normal case).
    Universal,
    /// Locally administered unicast (U/L bit set). Almost always a
    /// privacy-randomized MAC (iOS/Android/Windows), but also VMs, containers,
    /// and manually-set addresses. The OUI carries no vendor here.
    Local,
    /// Group address (I/G bit set) — multicast or the all-ones broadcast.
    /// Unusual for a host that answers ARP.
    Group,
}

/// Classify a `aa:bb:cc:…` / `aa-bb-cc-…` MAC by its first octet's U/L and I/G
/// bits. Returns `None` if the first octet can't be parsed as hex.
pub fn classify(mac: &str) -> Option<Kind> {
    let first = mac.split([':', '-']).next()?;
    let octet = u8::from_str_radix(first.trim(), 16).ok()?;
    Some(if octet & 0b0000_0001 != 0 {
        Kind::Group
    } else if octet & 0b0000_0010 != 0 {
        Kind::Local
    } else {
        Kind::Universal
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn universal_unicast() {
        // Apple OUI, U/L and I/G both clear.
        assert_eq!(classify("a4:83:e7:12:34:56"), Some(Kind::Universal));
    }

    #[test]
    fn locally_administered() {
        // Second hex digit 2/6/a/e => U/L set, I/G clear.
        for m in ["a2:00:00:00:00:01", "de:ad:be:ef:00:01", "06:11:22:33:44:55"] {
            assert_eq!(classify(m), Some(Kind::Local), "{m}");
        }
    }

    #[test]
    fn group_and_broadcast() {
        assert_eq!(classify("01:00:5e:00:00:fb"), Some(Kind::Group)); // multicast
        assert_eq!(classify("ff:ff:ff:ff:ff:ff"), Some(Kind::Group)); // broadcast
    }

    #[test]
    fn uppercase_and_bad_input() {
        assert_eq!(classify("A4:83:E7:00:00:01"), Some(Kind::Universal));
        assert_eq!(classify("zz:00"), None);
        assert_eq!(classify(""), None);
    }
}
