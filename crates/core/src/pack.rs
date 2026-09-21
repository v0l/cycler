use anyhow::{Context, Result, bail};
use battery_control::Reading;
use crate::chemistry::PackProfile;
use battery_control::{Battery, DeviceInfo};

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub cells_mv: Vec<u16>,
    pub balancing: Vec<usize>,
    pub pack_v: f64,
    pub current_a: f64,
    pub temp_c: f64,
    pub soc: u8,
    /// True when the SOC is derived from voltage rather than reported by a
    /// BMS, which matters: under load it can be badly wrong.
    pub soc_estimated: bool,
    /// Whatever the BMS is complaining about right now.
    pub alarms: Vec<String>,
    pub soh: Option<f64>,
    pub cycles: Option<f64>,
    /// Nameplate capacity, as the BMS reports it.
    pub rated_ah: Option<f64>,
}

impl Snapshot {
    /// Whether this pack reports cells at all.
    pub fn has_cells(&self) -> bool {
        !self.cells_mv.is_empty()
    }

    pub fn high_mv(&self) -> u16 {
        self.cells_mv.iter().copied().max().unwrap_or(0)
    }

    pub fn low_mv(&self) -> u16 {
        self.cells_mv.iter().copied().min().unwrap_or(0)
    }

    pub fn spread_mv(&self) -> u16 {
        self.high_mv().saturating_sub(self.low_mv())
    }

    pub fn high_cell(&self) -> usize {
        let hi = self.high_mv();
        self.cells_mv.iter().position(|c| *c == hi).unwrap_or(0)
    }

    pub fn low_cell(&self) -> usize {
        let lo = self.low_mv();
        self.cells_mv.iter().position(|c| *c == lo).unwrap_or(0)
    }

    /// What the pack is doing, from the sign of its own current.
    pub fn state_label(&self) -> &'static str {
        if self.current_a > 0.05 {
            "charging"
        } else if self.current_a < -0.05 {
            "discharging"
        } else {
            "idle"
        }
    }
}

pub use crate::discover::{Backend, Transport};

/// Every BMS `cycler` can read cells from. All of them arrive through
/// `battery-control`, so adding one is a feature flag and a match arm, not a
/// protocol. The adapter is whatever USB serial cable is wired to the BMS, so
/// no vendor ids are assumed.
pub const PACK_BACKENDS: &[Backend] = &[
    Backend {
        kind: "pylontech-console",
        label: "Pylontech console (US2000/3000C)",
        transport: Transport::Serial { usb: None },
    },
    Backend {
        kind: "pylontech-rs485",
        label: "Pylontech RS485 (US2000/3000)",
        transport: Transport::Serial { usb: None },
    },
    Backend {
        kind: "seplos",
        label: "Seplos V3 (RS485)",
        transport: Transport::Serial { usb: None },
    },
    Backend {
        kind: "pace",
        label: "PACE-BMS (RS485)",
        transport: Transport::Serial { usb: None },
    },
    Backend {
        kind: "daly",
        label: "Daly (serial)",
        transport: Transport::Serial { usb: None },
    },
    Backend {
        kind: "jk",
        label: "JK BMS (serial)",
        transport: Transport::Serial { usb: None },
    },
    Backend {
        kind: "jbd",
        label: "JBD / Overkill (serial)",
        transport: Transport::Serial { usb: None },
    },
    Backend {
        kind: "jk-ble",
        label: "JK BMS (Bluetooth)",
        transport: Transport::Ble,
    },
    Backend {
        kind: "jbd-ble",
        label: "JBD / Overkill (Bluetooth)",
        transport: Transport::Ble,
    },
    Backend {
        kind: "sok",
        label: "SOK (Bluetooth)",
        transport: Transport::Ble,
    },
    Backend {
        kind: "renogy",
        label: "Renogy (Bluetooth, BT-1/BT-2)",
        transport: Transport::Ble,
    },
    // Last: it is the fallback for a battery that cannot tell you anything,
    // not something to land on by accident.
    Backend {
        kind: "none",
        label: "No BMS: charge by pack voltage",
        transport: Transport::Manual,
    },
];

/// A battery whose individual cells can be read. Every charge and discharge
/// decision in `cycler` is made on cells, never on pack voltage, so cell
/// reporting is the one thing a pack must provide.
pub trait Pack {
    fn name(&self) -> String;
    fn read(&mut self) -> Result<Snapshot>;

    /// A pack with no BMS: there are no cells, and every limit has to be a
    /// pack-voltage limit. Lead-acid, a bare lithium pack, anything on a plain
    /// charger.
    fn blind(&self) -> bool {
        false
    }

    /// Tell the pack what it is. Only a pack with no BMS cares: it is the
    /// only way it can turn a terminal voltage into a state of charge.
    fn set_profile(&mut self, profile: PackProfile) {
        let _ = profile;
    }

    /// Nothing can see the battery any more: the supply is off and the load
    /// is disconnected. Holding the last reading would show a charge that
    /// stopped minutes ago as if it were still running.
    fn lost(&mut self) {}

    /// What the charger or load is measuring at its terminals. A blind pack
    /// has no other source of truth, so the run loop hands it the instrument
    /// reading before each read.
    fn observe(&mut self, volts: f64, amps: f64) {
        let _ = (volts, amps);
    }
}

/// Pick the instrument reading that actually describes the battery.
///
/// A supply measures its own output terminals, so it reads zero whenever its
/// output relay is open, no matter what is wired to it. A load measures its
/// input terminals, so it sees the battery the whole time it is connected.
/// Either can supply the current, but only the one that is delivering or
/// drawing knows it.
pub fn blind_reading(
    charger: Option<(f64, f64, bool)>,
    load: Option<(f64, f64, bool)>,
) -> Option<(f64, f64)> {
    let volts = load
        .map(|(v, _, _)| v)
        .filter(|v| *v > 0.5)
        .or_else(|| charger.map(|(v, _, _)| v).filter(|v| *v > 0.5))?;
    let amps = match (charger, load) {
        (_, Some((_, a, true))) => -a,
        (Some((_, a, true)), _) => a,
        _ => 0.0,
    };
    Some((volts, amps))
}

/// A battery that cannot report anything about itself. Its voltage and current
/// are whatever the charger or load measures at the terminals.
pub struct BlindPack {
    name: String,
    volts: f64,
    amps: f64,
    profile: PackProfile,
}

impl BlindPack {
    pub fn open(target: &str) -> Self {
        Self {
            name: if target.is_empty() {
                "battery (no BMS)".into()
            } else {
                target.to_string()
            },
            volts: 0.0,
            amps: 0.0,
            profile: PackProfile::default(),
        }
    }
}

impl Pack for BlindPack {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn blind(&self) -> bool {
        true
    }

    fn set_profile(&mut self, profile: PackProfile) {
        self.profile = profile;
    }

    fn observe(&mut self, volts: f64, amps: f64) {
        self.volts = volts;
        self.amps = amps;
    }

    fn lost(&mut self) {
        self.volts = 0.0;
        self.amps = 0.0;
    }

    fn read(&mut self) -> Result<Snapshot> {
        // SOC from the chemistry curve. Honest only at rest, so it is an
        // estimate and the UI says so.
        let soc = if self.volts > 0.5 {
            self.profile.soc_from_pack_v(self.volts).round() as u8
        } else {
            0
        };
        Ok(Snapshot {
            pack_v: self.volts,
            current_a: self.amps,
            soc,
            soc_estimated: true,
            rated_ah: Some(self.profile.capacity_ah()),
            ..Default::default()
        })
    }
}

/// Any `battery-control` backend, driven synchronously.
pub struct BcPack<B: Battery> {
    inner: B,
    rt: tokio::runtime::Runtime,
    info: DeviceInfo,
}

impl<B: Battery> BcPack<B> {
    pub fn new(inner: B, rt: tokio::runtime::Runtime) -> Result<Self> {
        let mut me = Self {
            info: inner.info().clone(),
            inner,
            rt,
        };
        me.read().context("first BMS read")?;
        me.info = me.inner.info().clone();
        Ok(me)
    }
}

impl<B: Battery> Pack for BcPack<B> {
    fn name(&self) -> String {
        match (&self.info.model, &self.info.serial) {
            (Some(m), Some(s)) => format!("{m} {s}"),
            (Some(m), None) => m.clone(),
            _ => self.info.backend.clone(),
        }
    }

    fn read(&mut self) -> Result<Snapshot> {
        let s = self.rt.block_on(self.inner.status())?;
        if s.cells.is_empty() {
            bail!("{} reports no cells", self.info.backend);
        }
        let cells_mv: Vec<u16> = s
            .cells
            .iter()
            .map(|c| (c.voltage.unwrap_or(0.0) * 1000.0).round() as u16)
            .collect();
        Ok(Snapshot {
            pack_v: s
                .get(Reading::Voltage)
                .unwrap_or_else(|| cells_mv.iter().map(|c| *c as f64).sum::<f64>() / 1000.0),
            balancing: s
                .cells
                .iter()
                .enumerate()
                .filter(|(_, c)| c.balancing.unwrap_or(false))
                .map(|(i, _)| i)
                .collect(),
            cells_mv,
            current_a: s.get(Reading::Current).unwrap_or(0.0),
            temp_c: s
                .reading("temp.pack")
                .or_else(|| s.temperature("temp.pack"))
                .unwrap_or(0.0),
            soc: s.soc().unwrap_or(0.0) as u8,
            soc_estimated: false,
            alarms: s.alarms.clone(),
            soh: s.get(Reading::Soh),
            cycles: s.get(Reading::Cycles),
            rated_ah: s.get(Reading::CapacityFullAh),
        })
    }
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?)
}

/// The RS485 protocols all default to the same address and rate; override with
/// `BMS_BAUD` and `BMS_ADDRESS` where a chain uses something else.
fn baud(default: u32) -> u32 {
    std::env::var("BMS_BAUD")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn address() -> u8 {
    std::env::var("BMS_ADDRESS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

pub fn open_pack(spec: &str) -> Result<Box<dyn Pack>> {
    use battery_control::backends;

    let (kind, target) = spec.split_once(':').unwrap_or((spec, ""));
    let backend = PACK_BACKENDS
        .iter()
        .find(|b| b.kind == kind)
        .or_else(|| {
            // Older specs named the console backend after the vendor alone.
            matches!(kind, "pylontech" | "pylontech-cli")
                .then(|| &PACK_BACKENDS[0])
        })
        .with_context(|| {
            format!(
                "unknown pack {kind:?}; known: {}",
                PACK_BACKENDS
                    .iter()
                    .map(|b| b.kind)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let owned;
    let path = if target.is_empty() && backend.kind != "none" {
        owned = backend.default_target()?;
        owned.as_str()
    } else {
        target
    };
    if backend.kind == "none" {
        return Ok(Box::new(BlindPack::open(target)));
    }
    let rt = runtime()?;
    match backend.kind {
        "pylontech-console" => {
            let bms = rt.block_on(backends::pylontech_cli::PylontechCli::open_serial(
                path,
                baud(115_200),
            ))?;
            Ok(Box::new(BcPack::new(bms, rt)?))
        }
        "pylontech-rs485" => {
            let bms = rt.block_on(backends::PylontechConsole::open_serial(
                path,
                baud(115_200),
                address(),
            ))?;
            Ok(Box::new(BcPack::new(bms, rt)?))
        }
        "seplos" => {
            let bms = rt.block_on(backends::SeplosBattery::open_serial(
                path,
                baud(19_200),
                address(),
            ))?;
            Ok(Box::new(BcPack::new(bms, rt)?))
        }
        "pace" => {
            let bms = rt.block_on(backends::PaceBattery::open_serial(
                path,
                baud(9_600),
                address(),
            ))?;
            Ok(Box::new(BcPack::new(bms, rt)?))
        }
        "daly" => {
            let bms = backends::DalyBattery::open_serial(path)?;
            Ok(Box::new(BcPack::new(bms, rt)?))
        }
        "jk" => {
            let bms = rt.block_on(backends::JkBattery::open_serial(path, baud(115_200)))?;
            Ok(Box::new(BcPack::new(bms, rt)?))
        }
        "jbd" => {
            let bms = rt.block_on(backends::JbdBattery::open_serial(path, baud(9_600)))?;
            Ok(Box::new(BcPack::new(bms, rt)?))
        }
        "jk-ble" => {
            let bms = rt.block_on(backends::JkBattery::connect_bluetooth(path))?;
            Ok(Box::new(BcPack::new(bms, rt)?))
        }
        "jbd-ble" => {
            let bms = rt.block_on(backends::JbdBattery::connect_bluetooth(path))?;
            Ok(Box::new(BcPack::new(bms, rt)?))
        }
        "sok" => {
            let bms = rt.block_on(backends::SokBattery::connect_bluetooth(path))?;
            Ok(Box::new(BcPack::new(bms, rt)?))
        }
        "renogy" => {
            let bms = rt.block_on(backends::RenogyBattery::connect_bluetooth(path))?;
            Ok(Box::new(BcPack::new(bms, rt)?))
        }
        other => bail!("pack {other:?} is listed but not wired up"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(cells: &[u16]) -> Snapshot {
        Snapshot {
            cells_mv: cells.to_vec(),
            ..Default::default()
        }
    }

    #[test]
    fn derives_cell_metrics() {
        let s = snap(&[3331, 3400, 3350]);
        assert_eq!(s.high_mv(), 3400);
        assert_eq!(s.low_mv(), 3331);
        assert_eq!(s.spread_mv(), 69);
        assert_eq!(s.high_cell(), 1);
        assert_eq!(s.low_cell(), 0);
    }

    #[test]
    fn a_blind_pack_forgets_a_reading_nothing_can_still_see() {
        let mut p = BlindPack::open("");
        p.observe(15.0, 2.0);
        assert_eq!(p.read().unwrap().current_a, 2.0);
        p.lost();
        let s = p.read().unwrap();
        assert_eq!(s.current_a, 0.0);
        assert_eq!(s.pack_v, 0.0);
        assert_eq!(s.state_label(), "idle");
    }

    #[test]
    fn a_blind_pack_is_read_by_whichever_instrument_can_see_it() {
        // Supply off (reads its own open terminals as 0 V), load connected
        // and idle: the load is the only one looking at the battery.
        assert_eq!(
            blind_reading(Some((0.0, 0.0, false)), Some((15.0, 0.0, false))),
            Some((15.0, 0.0))
        );
        // Charging: the supply reads the battery and knows the current.
        assert_eq!(
            blind_reading(Some((14.4, 6.0, true)), Some((14.4, 0.0, false))),
            Some((14.4, 6.0))
        );
        // Discharging: current leaves the pack, so it is negative to it.
        assert_eq!(
            blind_reading(Some((0.0, 0.0, false)), Some((12.8, 3.0, true))),
            Some((12.8, -3.0))
        );
        // Nothing connected at all.
        assert_eq!(blind_reading(Some((0.0, 0.0, false)), None), None);
    }

    #[test]
    fn labels_state_from_current_sign() {
        let mut s = snap(&[3300]);
        assert_eq!(s.state_label(), "idle");
        s.current_a = 1.2;
        assert_eq!(s.state_label(), "charging");
        s.current_a = -1.2;
        assert_eq!(s.state_label(), "discharging");
    }

    #[test]
    fn empty_pack_does_not_underflow() {
        let s = snap(&[]);
        assert_eq!(s.spread_mv(), 0);
        assert_eq!(s.high_cell(), 0);
    }
}
