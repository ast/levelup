//! A tiny MAC-keyed SQLite cache at `$XDG_CACHE_HOME/heimdall/devices.db`. It's
//! a *speed-up*, not a history: on launch we seed the picker with last-known
//! devices (shown dimmed as "checking…") so relaunch is instant, and we skip
//! re-resolving names we already have. The live engine then confirms or the
//! liveness timeout drops them. Best-effort throughout — any failure just means
//! "no cache".

use std::net::Ipv4Addr;
use std::path::PathBuf;

use rusqlite::{Connection, params};

use crate::device::Device;

pub struct Cached {
    pub ip: Ipv4Addr,
    pub mac: String,
    pub vendor: Option<String>,
    pub hostname: Option<String>,
    pub services: Vec<String>,
}

/// Open (and create) the cache DB. `None` on any failure — Heimdall just runs
/// without a cache.
pub fn open() -> Option<Connection> {
    let path = default_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    let conn = Connection::open(&path).ok()?;
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         CREATE TABLE IF NOT EXISTS devices (
             mac       TEXT PRIMARY KEY,
             ip        TEXT NOT NULL,
             vendor    TEXT,
             hostname  TEXT,
             services  TEXT,
             last_seen INTEGER NOT NULL
         );",
    )
    .ok()?;
    Some(conn)
}

/// Load the most-recently-seen cached devices (newest first).
pub fn load(conn: &Connection) -> Vec<Cached> {
    let mut out = Vec::new();
    let Ok(mut stmt) = conn.prepare(
        "SELECT mac, ip, vendor, hostname, services FROM devices ORDER BY last_seen DESC LIMIT 1000",
    ) else {
        return out;
    };
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<String>>(4)?,
        ))
    });
    if let Ok(rows) = rows {
        for (mac, ip_s, vendor, hostname, services) in rows.flatten() {
            if let Ok(ip) = ip_s.parse::<Ipv4Addr>() {
                let services = services
                    .map(|s| {
                        s.split(',')
                            .filter(|x| !x.is_empty())
                            .map(String::from)
                            .collect()
                    })
                    .unwrap_or_default();
                out.push(Cached {
                    ip,
                    mac,
                    vendor,
                    hostname,
                    services,
                });
            }
        }
    }
    out
}

/// Upsert the devices that have a MAC (the stable identity). `last_seen` is unix
/// seconds. Done in one transaction.
pub fn save(conn: &Connection, devices: &[(&Device, i64)]) {
    let _ = conn.execute_batch("BEGIN;");
    for (d, last_seen) in devices {
        if let Some(mac) = &d.mac {
            let services = d.services.join(",");
            let _ = conn.execute(
                "INSERT INTO devices(mac, ip, vendor, hostname, services, last_seen)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(mac) DO UPDATE SET
                     ip=?2, vendor=COALESCE(?3, vendor), hostname=COALESCE(?4, hostname),
                     services=?5, last_seen=?6",
                params![
                    mac,
                    d.ip.to_string(),
                    d.vendor,
                    d.hostname,
                    services,
                    last_seen
                ],
            );
        }
    }
    let _ = conn.execute_batch("COMMIT;");
}

fn default_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("heimdall").join("devices.db"))
}
