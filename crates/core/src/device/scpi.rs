//! A bare SCPI serial link, for identifying an instrument and finding out
//! which commands it actually implements before a backend is written for it.

use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::time::{Duration, Instant};

pub struct Scpi {
    port: Box<dyn serialport::SerialPort>,
    last_write: Option<Instant>,
}

const WRITE_GAP: Duration = Duration::from_millis(50);

/// Queries worth trying on an unknown electronic load. Harmless: every one is
/// a read, and an instrument ignores what it does not know.
pub const LOAD_PROBES: &[&str] = &[
    "*IDN?",
    "SYST:ERR?",
    "SYST:VERS?",
    ":INP?",
    ":INP:STAT?",
    ":FUNC?",
    ":SOUR:FUNC?",
    ":CURR?",
    ":SOUR:CURR?",
    ":VOLT?",
    ":RES?",
    ":POW?",
    ":MEAS:VOLT?",
    ":MEAS:CURR?",
    ":MEAS:POW?",
    ":MEAS:RES?",
    ":BATT:CAP?",
    ":BATT:TIM?",
    ":BATT:VSTOP?",
    ":CURR:PROT?",
    ":VOLT:PROT?",
    ":POW:PROT?",
];

impl Scpi {
    pub fn open(path: &str, baud: u32) -> Result<Self> {
        let port = serialport::new(path, baud)
            .timeout(Duration::from_millis(400))
            .open()
            .with_context(|| format!("opening {path} at {baud}"))?;
        Ok(Self {
            port,
            last_write: None,
        })
    }

    pub fn send(&mut self, cmd: &str) -> Result<()> {
        if let Some(at) = self.last_write {
            std::thread::sleep(WRITE_GAP.saturating_sub(at.elapsed()));
        }
        self.port.write_all(format!("{cmd}\r\n").as_bytes())?;
        self.port.flush()?;
        self.last_write = Some(Instant::now());
        Ok(())
    }

    pub fn ask(&mut self, cmd: &str) -> Result<String> {
        let _ = self.port.clear(serialport::ClearBuffer::Input);
        self.send(cmd)?;
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        let deadline = Instant::now() + Duration::from_millis(700);
        while Instant::now() < deadline {
            match self.port.read(&mut byte) {
                Ok(1) => {
                    if byte[0] == b'\n' {
                        break;
                    }
                    if byte[0] != b'\r' {
                        buf.push(byte[0]);
                    }
                }
                _ => continue,
            }
        }
        Ok(String::from_utf8_lossy(&buf).trim().to_string())
    }

    pub fn identify(&mut self) -> Result<String> {
        let id = self.ask("*IDN?")?;
        if id.is_empty() {
            bail!("no reply to *IDN?: check baud, and that COMM PRO is SCPI not Modbus");
        }
        Ok(id)
    }
}
