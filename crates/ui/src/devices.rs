use cycler_core::discover::{Backend, Candidate, Transport};
use std::sync::mpsc::{Receiver, channel};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// One role's choice: which backend drives it, and which of the things found
/// on this host it should open.
pub struct Choice {
    pub role: &'static str,
    pub backends: &'static [Backend],
    pub backend: usize,
    pub candidates: Vec<Candidate>,
    pub target: usize,
    pub enabled: bool,
    /// A scan in flight. Bluetooth discovery takes seconds, so it runs on its
    /// own thread and the UI keeps painting while it does.
    pending: Option<Receiver<Vec<Candidate>>>,
    keep: Option<String>,
}

impl Choice {
    pub fn new(role: &'static str, backends: &'static [Backend]) -> Self {
        let mut c = Self {
            role,
            backends,
            backend: 0,
            candidates: Vec::new(),
            target: 0,
            enabled: true,
            pending: None,
            keep: None,
        };
        c.rescan();
        c
    }

    pub fn backend(&self) -> Option<&Backend> {
        self.backends.get(self.backend)
    }

    /// Whether this backend has a port to choose at all. A blind pack or a
    /// load switched by hand has nothing to enumerate, so offering a picker
    /// with one meaningless entry is just noise.
    pub fn has_ports(&self) -> bool {
        self.backend()
            .map(|b| b.transport != Transport::Manual)
            .unwrap_or(false)
    }

    pub fn scanning(&self) -> bool {
        self.pending.is_some()
    }

    pub fn rescan(&mut self) {
        let keep = self.selected().map(|c| c.target.clone());
        let Some(backend) = self.backend().copied() else {
            self.candidates.clear();
            return;
        };
        // Enumerating serial ports is instant; a radio scan is not.
        if backend.transport == Transport::Ble {
            self.keep = keep;
            self.candidates.clear();
            self.target = 0;
            let (tx, rx) = channel();
            std::thread::spawn(move || {
                let _ = tx.send(backend.candidates());
            });
            self.pending = Some(rx);
            return;
        }
        self.candidates = backend.candidates();
        self.target = keep
            .and_then(|t| self.candidates.iter().position(|c| c.target == t))
            .unwrap_or(0);
    }

    /// Collect a finished scan. Call once a frame.
    pub fn poll(&mut self) {
        let Some(rx) = self.pending.as_ref() else {
            return;
        };
        if let Ok(found) = rx.try_recv() {
            self.candidates = found;
            self.target = self
                .keep
                .take()
                .and_then(|t| self.candidates.iter().position(|c| c.target == t))
                .unwrap_or(0);
            self.pending = None;
        }
    }

    pub fn selected(&self) -> Option<&Candidate> {
        self.candidates.get(self.target)
    }

    /// The spec string to hand to `open_*`, or `None` when this role is off or
    /// has nothing to open.
    pub fn spec(&self) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let b = self.backend()?;
        Some(b.spec(self.selected().map(|c| c.target.as_str()).unwrap_or("")))
    }

    /// Re-select what was used last time. A remembered port that is not
    /// plugged in right now is kept in the list and marked, rather than
    /// silently falling back to a different device.
    fn restore(&mut self, spec: &str) {
        let (kind, target) = spec.split_once(':').unwrap_or((spec, ""));
        if let Some(i) = self.backends.iter().position(|b| b.kind == kind)
            && i != self.backend
        {
            self.backend = i;
            self.candidates = self.backends[i].candidates();
        }
        if target.is_empty() {
            return;
        }
        match self.candidates.iter().position(|c| c.target == target) {
            Some(i) => self.target = i,
            None => {
                self.candidates.push(Candidate {
                    label: format!("{target} (not present)"),
                    target: target.to_string(),
                    matches_ids: false,
                });
                self.target = self.candidates.len() - 1;
            }
        }
    }
}

/// What was selected last time, so a rig that is wired one way stays wired
/// that way across restarts. Two identical USB adapters cannot be told apart
/// by enumeration, so the choice has to be remembered rather than guessed.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Remembered {
    #[serde(default)]
    pub specs: BTreeMap<String, String>,
    #[serde(default)]
    pub disabled: Vec<String>,
    /// What the battery is. Kept with the device selection because it is the
    /// same kind of fact: a property of the rig, not of a run.
    #[serde(default)]
    pub profile: Option<cycler_core::chemistry::PackProfile>,
    /// Test rate as a fraction of capacity.
    #[serde(default)]
    pub c_rate: Option<f64>,
}

fn config_path() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("cycler").join("devices.json"))
}

impl Remembered {
    pub fn load() -> Self {
        config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) {
        let Some(path) = config_path() else { return };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(path, text);
        }
    }

    pub fn apply(&self, picks: [&mut Choice; 3]) {
        for pick in picks {
            if let Some(spec) = self.specs.get(pick.role) {
                pick.restore(spec);
            }
            if self.disabled.iter().any(|r| r == pick.role) {
                pick.enabled = false;
            }
        }
    }

    pub fn remember(&mut self, picks: [&Choice; 3]) {
        for pick in picks {
            if let Some(b) = pick.backend() {
                let target = pick.selected().map(|c| c.target.as_str()).unwrap_or("");
                self.specs.insert(pick.role.to_string(), b.spec(target));
            }
            self.disabled.retain(|r| r != pick.role);
            if !pick.enabled {
                self.disabled.push(pick.role.to_string());
            }
        }
        self.save();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cycler_core::pack::PACK_BACKENDS;

    #[test]
    fn a_remembered_port_that_is_gone_is_kept_and_marked() {
        let mut c = Choice::new("battery", PACK_BACKENDS);
        c.restore("pylontech-console:/dev/ttyNOPE");
        assert_eq!(c.selected().unwrap().target, "/dev/ttyNOPE");
        assert!(c.selected().unwrap().label.contains("not present"));
        assert_eq!(
            c.spec().as_deref(),
            Some("pylontech-console:/dev/ttyNOPE")
        );
    }

    #[test]
    fn rescan_keeps_the_current_target_when_it_is_still_there() {
        let mut c = Choice::new("battery", PACK_BACKENDS);
        if c.candidates.len() > 1 {
            c.target = 1;
            let want = c.selected().unwrap().target.clone();
            c.rescan();
            assert_eq!(c.selected().unwrap().target, want);
        }
    }
}
