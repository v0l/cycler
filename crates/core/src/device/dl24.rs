//! Atorch DL24/DL24P over its **USB HID** interface.
//!
//! This is not the `FF 55` serial/Bluetooth Atorch protocol. The USB build
//! never pushes anything: the host polls with `55 05 <type> <sub> <data> EE FF`
//! in a 64-byte report and the device answers `AA 05 ...`. It also stays
//! completely mute until it receives the HID SET_IDLE request (which Windows
//! sends during enumeration and Linux does not) followed by an init sweep of
//! sub-command `0x04` across command types `0x01..=0x0a`.

use super::{Device, Discharger, Limits, LoadMode, LoadState, Sample};
use anyhow::{Context, Result, bail};
use nusb::MaybeFuture;
use nusb::transfer::{ControlOut, ControlType, In, Interrupt, Out, Recipient};
use std::io::{Read, Write};
use std::thread::sleep;
use std::time::{Duration, Instant};

pub const VID: u16 = 0x0483;
pub const PID: u16 = 0x5750;

const EP_OUT: u8 = 0x01;
const EP_IN: u8 = 0x81;
const REPORT: usize = 64;

const CMD_HEADER: u8 = 0x55;
const RESP_HEADER: u8 = 0xAA;
const PROTO: u8 = 0x05;
const TYPE_QUERY: u8 = 0x01;

const SUB_LIVE: u8 = 0x03;
const SUB_INIT: u8 = 0x04;
const SUB_COUNTERS: u8 = 0x05;
const SUB_SET_VALUE: u8 = 0x21;
/// Mode select. A value means nothing until the load knows which mode it is
/// setting, which is why a current alone leaves it drawing zero.
const SUB_MODE_CC: u8 = 0x47;
const SUB_MODE_CV: u8 = 0x48;
const SUB_MODE_CR: u8 = 0x49;
const SUB_MODE_CP: u8 = 0x4A;

fn mode_sub(mode: LoadMode) -> u8 {
    match mode {
        LoadMode::Cc => SUB_MODE_CC,
        LoadMode::Cv => SUB_MODE_CV,
        LoadMode::Cr => SUB_MODE_CR,
        LoadMode::Cp => SUB_MODE_CP,
    }
}
const SUB_POWER: u8 = 0x25;
const SUB_CUTOFF: u8 = 0x29;
const SUB_CLEAR: u8 = 0x34;

/// The OEM app sends these three bytes with every query.
const QUERY_TAG: [u8; 3] = [0x0B, 0x00, 0x8C];

pub struct Dl24 {
    reader: nusb::io::EndpointRead<Interrupt>,
    writer: nusb::io::EndpointWrite<Interrupt>,
    name: String,
    /// Ratings. The DL24 family shares one USB id across very different
    /// models, and the payload field below is inferred from captures rather
    /// than a spec, so every one of these can be overridden:
    /// `DL24_MAX_V`, `DL24_MAX_A`, `DL24_MAX_W`.
    max_volts: f64,
    max_amps: f64,
    max_watts: f64,
}

fn env_limit(key: &str, fallback: f64) -> f64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(fallback)
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Counters {
    pub volts: f64,
    pub amps: f64,
    pub watts: f64,
    /// What the load is seeing. The field is thousandths of an ohm, and it
    /// pegs at 9999.991 with nothing flowing.
    pub ohms: f64,
    pub watt_hours: f64,
    pub amp_hours: f64,
    pub runtime_s: f64,
    pub mosfet_temp_c: f64,
    pub load_on: bool,
}

fn u16le(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32le(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub fn build(cmd_type: u8, sub: u8, data: &[u8]) -> [u8; REPORT] {
    let mut p = [0u8; REPORT];
    p[0] = CMD_HEADER;
    p[1] = PROTO;
    p[2] = cmd_type;
    p[3] = sub;
    let end = 4 + data.len().min(REPORT - 6);
    p[4..end].copy_from_slice(&data[..end - 4]);
    p[end] = 0xEE;
    p[end + 1] = 0xFF;
    p
}

/// Decode the counters reply (sub-command `0x05`), which carries the measured
/// values as little-endian integers.
pub fn decode_counters(resp: &[u8]) -> Option<Counters> {
    if resp.len() < 62 || resp[0] != RESP_HEADER {
        return None;
    }
    let p = &resp[4..62];
    Some(Counters {
        volts: u16le(p, 4) as f64 / 1000.0,
        amps: u16le(p, 8) as f64 / 1000.0,
        watts: u16le(p, 12) as f64 / 1000.0,
        ohms: u32le(p, 16) as f64 / 1000.0,
        watt_hours: u32le(p, 20) as f64 / 1000.0,
        amp_hours: u32le(p, 24) as f64 / 1_000_000.0,
        runtime_s: u32le(p, 28) as f64 / 12.0,
        mosfet_temp_c: u32le(p, 36) as f64 / 1000.0,
        load_on: p[48] != 0,
    })
}

impl Dl24 {
    fn open_after_reset() -> Result<Self> {
        let mut dev = Self::claim()?;
        dev.init()?;
        dev.counters().context("DL24 still mute after a USB reset")?;
        dev.read_rating();
        Ok(dev)
    }

    /// Input voltage rating, from the live-data payload. Skipped when the
    /// caller has stated it, because this field's meaning is a guess: it read
    /// 36 on a 36 V unit, which is one sample, not a spec.
    fn read_rating(&mut self) {
        if std::env::var("DL24_MAX_V").is_ok() {
            return;
        }
        if let Ok(Some(r)) = self.live_raw()
            && let Some(b) = r.get(24..28)
        {
            let v = f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64;
            if (10.0..=500.0).contains(&v) {
                self.max_volts = v;
            }
        }
    }

    /// A USB port reset, which is the only thing that revives the firmware
    /// once it stops answering.
    pub fn reset_usb() -> Result<()> {
        let info = nusb::list_devices()
            .wait()?
            .find(|d| d.vendor_id() == VID && d.product_id() == PID)
            .context("DL24 not on the USB bus")?;
        let device = info.open().wait().context("opening DL24")?;
        device.reset().wait().context("USB reset")?;
        std::thread::sleep(Duration::from_millis(1200));
        Ok(())
    }

    pub fn open(_target: &str) -> Result<Self> {
        let mut dev = Self::claim()?;
        dev.init()?;
        if dev.counters().is_err() {
            drop(dev);
            Self::reset_usb()?;
            return Self::open_after_reset();
        }
        dev.read_rating();
        Ok(dev)
    }

    fn claim() -> Result<Self> {
        let info = nusb::list_devices()
            .wait()?
            .find(|d| d.vendor_id() == VID && d.product_id() == PID)
            .context("DL24 not on the USB bus")?;
        let name = info.product_string().unwrap_or("ATORCH DL24").to_string();
        let device = info.open().wait().context("opening DL24")?;
        let interface = device
            .detach_and_claim_interface(0)
            .wait()
            .context("claiming DL24 interface 0")?;

        interface
            .control_out(
                ControlOut {
                    control_type: ControlType::Class,
                    recipient: Recipient::Interface,
                    request: 0x0A,
                    value: 0x0000,
                    index: 0,
                    data: &[],
                },
                Duration::from_millis(500),
            )
            .wait()
            .context("SET_IDLE")?;

        let reader = interface
            .endpoint::<Interrupt, In>(EP_IN)?
            .reader(REPORT)
            .with_num_transfers(4)
            .with_read_timeout(Duration::from_millis(600));
        let writer = interface
            .endpoint::<Interrupt, Out>(EP_OUT)?
            .writer(REPORT)
            .with_num_transfers(2);
        Ok(Self {
            reader,
            writer,
            name,
            max_volts: env_limit("DL24_MAX_V", 36.0),
            max_amps: env_limit("DL24_MAX_A", 20.0),
            max_watts: env_limit("DL24_MAX_W", 150.0),
        })
    }

    fn init(&mut self) -> Result<()> {
        for cmd_type in 0x01..=0x0A {
            self.write_report(&build(cmd_type, SUB_INIT, &[0, 0, 0, 0]))?;
            sleep(Duration::from_millis(160));
        }
        sleep(Duration::from_millis(600));
        Ok(())
    }

    fn write_report(&mut self, report: &[u8; REPORT]) -> Result<()> {
        self.writer.write_all(report)?;
        self.writer.flush()?;
        Ok(())
    }

    fn ask(&mut self, sub: u8, data: &[u8]) -> Result<Option<Vec<u8>>> {
        self.write_report(&build(TYPE_QUERY, sub, data))?;
        let deadline = Instant::now() + Duration::from_millis(800);
        let mut buf = [0u8; REPORT];
        while Instant::now() < deadline {
            match self.reader.read(&mut buf) {
                Ok(n) if n > 0 && buf[0] == RESP_HEADER => return Ok(Some(buf[..n].to_vec())),
                Ok(_) => continue,
                Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
                Err(e) => bail!("DL24 read: {e}"),
            }
        }
        Ok(None)
    }

    fn set(&mut self, sub: u8, data: &[u8]) -> Result<()> {
        self.write_report(&build(TYPE_QUERY, sub, data))
    }

    pub fn counters(&mut self) -> Result<Counters> {
        match self.ask(SUB_COUNTERS, &QUERY_TAG)? {
            Some(r) => decode_counters(&r).context("counters reply did not decode"),
            None => bail!("DL24 did not answer the counters query"),
        }
    }

    pub fn live_raw(&mut self) -> Result<Option<Vec<u8>>> {
        self.ask(SUB_LIVE, &QUERY_TAG)
    }

    /// The setpoint the load says it is holding, as a big-endian float at the
    /// start of the live-data payload. Worth reading back: a value sent in the
    /// wrong mode is accepted and ignored.
    pub fn setpoint(&mut self) -> Result<Option<f64>> {
        Ok(self.live_raw()?.and_then(|r| {
            r.get(4..8)
                .map(|b| f32::from_be_bytes([b[0], b[1], b[2], b[3]]) as f64)
        }))
    }

    /// Clear the accumulated mAh, Wh and runtime totals. Call before a test.
    pub fn reset_counters(&mut self) -> Result<()> {
        self.set(SUB_CLEAR, &[0, 0, 0, 0])
    }

    /// Cutoff voltage below which the load stops by itself, a hardware backstop
    /// independent of the controller.
    pub fn set_cutoff_volts(&mut self, volts: f32) -> Result<()> {
        self.set(SUB_CUTOFF, &volts.to_be_bytes())
    }
}

impl Device for Dl24 {
    fn name(&self) -> String {
        self.name.clone()
    }

    fn limits(&self) -> Limits {
        Limits {
            max_volts: self.max_volts,
            max_amps: self.max_amps,
            max_watts: self.max_watts,
        }
    }

    fn measure(&mut self) -> Result<Sample> {
        let c = self.counters()?;
        Ok(Sample {
            volts: c.volts,
            amps: c.amps,
        })
    }

    fn stop(&mut self) -> Result<()> {
        self.set(SUB_POWER, &[0x00, 0x00, 0x00, 0x00])
    }

    fn output_on(&mut self) -> Result<Option<bool>> {
        Ok(Some(self.counters()?.load_on))
    }
}

impl Discharger for Dl24 {
    fn modes(&self) -> &'static [LoadMode] {
        &[LoadMode::Cc, LoadMode::Cp, LoadMode::Cv, LoadMode::Cr]
    }

    fn set_mode(&mut self, mode: LoadMode, value: f64) -> Result<()> {
        // Mode first: the value command is the same in every mode, and the
        // load reads it as whatever mode it is currently in.
        self.set(mode_sub(mode), &[0, 0, 0, 0])?;
        std::thread::sleep(Duration::from_millis(160));
        self.set(SUB_SET_VALUE, &(value as f32).to_be_bytes())?;
        std::thread::sleep(Duration::from_millis(160));
        Ok(())
    }

    fn set_cutoff_volts(&mut self, volts: f64) -> Result<bool> {
        Dl24::set_cutoff_volts(self, volts as f32)?;
        std::thread::sleep(Duration::from_millis(160));
        Ok(true)
    }

    fn set_current(&mut self, amps: f64) -> Result<()> {
        if amps > self.max_amps {
            bail!("{amps:.2} A is over this load's {:.0} A rating", self.max_amps);
        }
        self.set_mode(LoadMode::Cc, amps)
    }

    fn as_any(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn start(&mut self) -> Result<()> {
        let v = self.counters()?.volts;
        if v > self.max_volts {
            bail!(
                "{v:.1} V is over this load's {:.0} V input rating; it will sink nothing",
                self.max_volts
            );
        }
        self.set(SUB_POWER, &[0x01, 0x00, 0x00, 0x00])
    }

    fn amp_hours(&mut self) -> Result<Option<f64>> {
        Ok(Some(self.counters()?.amp_hours))
    }

    fn state(&mut self) -> Result<LoadState> {
        let c = self.counters()?;
        let setpoint = self.setpoint()?.unwrap_or(0.0);
        Ok(LoadState {
            setpoint,
            volts: c.volts,
            amps: c.amps,
            watts: c.watts,
            amp_hours: c.amp_hours,
            watt_hours: c.watt_hours,
            ohms: Some(c.ohms),
            temp_c: c.mosfet_temp_c,
            runtime_s: c.runtime_s,
            on: c.load_on,
        })
    }
}

impl Drop for Dl24 {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const COUNTERS: &str = "aa050105000000004dc90000000000000000000077969800fb10010022ce3800487100006fae0000e36b0000000000000000000000000000000000002000eeff";

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn decodes_counters_from_device_capture() {
        let c = decode_counters(&hex(COUNTERS)).expect("counters");
        assert!((c.volts - 51.533).abs() < 1e-9);
        assert_eq!(c.amps, 0.0);
        assert!((c.ohms - 9999.991).abs() < 1e-9);
        assert!((c.watt_hours - 69.883).abs() < 1e-9);
        assert!((c.amp_hours - 3.722786).abs() < 1e-9);
        assert!((c.mosfet_temp_c - 27.619).abs() < 1e-9);
        // Against a running discharge: the field read 32768 with the front
        // panel at 45:32 and 1.0922 Ah taken at 1.499 A, which is 2732 s.
        assert!((c.runtime_s - 29000.0 / 12.0).abs() < 1e-9);
        assert!(!c.load_on);
    }

    #[test]
    fn builds_query_frame() {
        let p = build(TYPE_QUERY, SUB_COUNTERS, &QUERY_TAG);
        assert_eq!(&p[..7], &[0x55, 0x05, 0x01, 0x05, 0x0B, 0x00, 0x8C]);
        assert_eq!(&p[7..9], &[0xEE, 0xFF]);
    }

    #[test]
    fn set_current_uses_big_endian_float() {
        let p = build(TYPE_QUERY, SUB_SET_VALUE, &3.0f32.to_be_bytes());
        assert_eq!(&p[..4], &[0x55, 0x05, 0x01, 0x21]);
        assert_eq!(&p[4..8], &3.0f32.to_be_bytes());
    }
}
