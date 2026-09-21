use anyhow::{Context, Result};
use nusb::MaybeFuture;

/// How a backend is reached, and therefore how its candidates are found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// A serial port. Candidates come from the host's port list.
    Serial { usb: Option<(u16, u16)> },
    /// A raw USB device, addressed by vendor and product id.
    Usb { vid: u16, pid: u16 },
    /// Bluetooth LE. Candidates come from a scan, so asking for them costs
    /// seconds, not microseconds.
    Ble,
    /// Nothing to enumerate: the operator says what is wired up.
    Manual,
}

/// One selectable backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backend {
    pub kind: &'static str,
    pub label: &'static str,
    pub transport: Transport,
}

/// Something a backend could be opened on, found on this machine now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// What to put after the colon in a spec.
    pub target: String,
    /// What to show a person.
    pub label: String,
    /// Whether the device identifies as the one this backend drives.
    pub matches_ids: bool,
}

impl Backend {
    pub fn spec(&self, target: &str) -> String {
        format!("{}:{}", self.kind, target)
    }

    /// Everything on this host the backend could plausibly talk to, best
    /// guesses first. Never an error: an empty list means nothing is plugged
    /// in, which the caller reports better than a failure does.
    pub fn candidates(&self) -> Vec<Candidate> {
        match self.transport {
            Transport::Serial { usb } => serial_candidates(usb),
            Transport::Usb { vid, pid } => usb_candidates(vid, pid),
            Transport::Ble => ble_candidates(),
            Transport::Manual => vec![Candidate {
                target: String::new(),
                label: "wired by hand".into(),
                matches_ids: true,
            }],
        }
    }

    /// The target to use when the caller named none.
    pub fn default_target(&self) -> Result<String> {
        self.candidates()
            .into_iter()
            .next()
            .map(|c| c.target)
            .with_context(|| format!("nothing found for {}: plug it in or name a target", self.kind))
    }
}

/// Prefer the stable `by-id` path over `ttyUSBn`, which moves between boots.
fn by_id_path(port: &str) -> String {
    let dir = std::path::Path::new("/dev/serial/by-id");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return port.to_string();
    };
    for e in entries.flatten() {
        if let Ok(real) = std::fs::canonicalize(e.path())
            && real.to_string_lossy() == port
        {
            return e.path().to_string_lossy().into_owned();
        }
    }
    port.to_string()
}

fn serial_candidates(want: Option<(u16, u16)>) -> Vec<Candidate> {
    let Ok(ports) = serialport::available_ports() else {
        return Vec::new();
    };
    let mut out: Vec<Candidate> = ports
        .into_iter()
        .map(|p| {
            let (ids, desc) = match &p.port_type {
                serialport::SerialPortType::UsbPort(info) => (
                    Some((info.vid, info.pid)),
                    format!(
                        "{} {}",
                        info.manufacturer.clone().unwrap_or_default(),
                        info.product.clone().unwrap_or_default()
                    )
                    .trim()
                    .to_string(),
                ),
                _ => (None, String::new()),
            };
            let path = by_id_path(&p.port_name);
            let short = path.rsplit('/').next().unwrap_or(&path).to_string();
            Candidate {
                matches_ids: ids.is_some() && (want.is_none() || ids == want),
                label: if desc.is_empty() {
                    short
                } else {
                    format!("{desc} ({short})")
                },
                target: path,
            }
        })
        .collect();
    // Wanted ids first, then any USB adapter, and the motherboard's own
    // ttyS ports last: they are almost never the thing.
    out.sort_by_key(|c| (!c.matches_ids, c.target.contains("ttyS")));
    out
}

/// Scan for Bluetooth batteries. `battery-control` already knows which
/// advertisements belong to which backend, so this asks it rather than
/// matching names here.
fn ble_candidates() -> Vec<Candidate> {
    let secs: u64 = std::env::var("BLE_SCAN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(6);
    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return Vec::new();
    };
    let opts = battery_control::discovery::DiscoverOptions {
        ble_secs: secs,
        // Serial ports are enumerated separately and probing them here would
        // open devices the user did not ask for.
        probe_serial: false,
        ..Default::default()
    };
    match rt.block_on(battery_control::discovery::discover(&opts)) {
        Ok(found) => found
            .into_iter()
            .filter(|d| d.id.starts_with("ble:"))
            .map(|d| Candidate {
                target: d.id.trim_start_matches("ble:").to_string(),
                label: format!("{} ({})", d.label, d.backend),
                matches_ids: true,
            })
            .collect(),
        Err(e) => {
            eprintln!("ble scan: {e}");
            Vec::new()
        }
    }
}

fn usb_candidates(vid: u16, pid: u16) -> Vec<Candidate> {
    let Ok(devices) = nusb::list_devices().wait() else {
        return Vec::new();
    };
    devices
        .filter(|d| d.vendor_id() == vid && d.product_id() == pid)
        .map(|d| Candidate {
            target: format!("{:04x}:{:04x}", d.vendor_id(), d.product_id()),
            label: d
                .product_string()
                .map(|s| s.to_string())
                .unwrap_or_else(|| format!("{vid:04x}:{pid:04x}")),
            matches_ids: true,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_joins_kind_and_target() {
        let b = Backend {
            kind: "owon",
            label: "OWON",
            transport: Transport::Serial { usb: None },
        };
        assert_eq!(b.spec("/dev/ttyUSB3"), "owon:/dev/ttyUSB3");
    }

    #[test]
    fn enumerating_never_panics_on_this_host() {
        // Bluetooth is excluded on purpose: a scan takes seconds and needs a
        // radio, neither of which belongs in a unit test.
        for b in crate::pack::PACK_BACKENDS
            .iter()
            .filter(|b| b.transport != Transport::Ble)
        {
            let _ = b.candidates();
        }
    }
}
