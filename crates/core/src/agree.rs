//! Do the instruments agree about the battery they are wired to?
//!
//! Three things in the rig can measure the same terminals: the BMS, the
//! supply and the load. When they disagree, one of them is not connected to
//! the battery the other two are, and every limit downstream is being applied
//! to the wrong thing. That is worth stopping for.

/// One instrument's view of the battery.
#[derive(Debug, Clone, Copy)]
pub struct Instrument {
    pub who: &'static str,
    /// What it reads at its own terminals, if it can read at all.
    pub volts: Option<f64>,
    /// Whether it is delivering or sinking right now, which is when its
    /// reading has to be believed and when a dead reading is a fault.
    pub live: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Mismatch {
    /// An instrument that is switched on sees no battery at all.
    Disconnected { who: &'static str },
    /// Two instruments disagree about the same battery.
    Disagree {
        who: &'static str,
        theirs: f64,
        pack: f64,
    },
}

impl Mismatch {
    pub fn message(&self) -> String {
        match self {
            Mismatch::Disconnected { who } => format!(
                "{who} is on but sees no voltage: check the leads, the fuse and the breaker"
            ),
            Mismatch::Disagree { who, theirs, pack } => format!(
                "{who} sees {theirs:.2} V but the battery reads {pack:.2} V: \
                 they are not on the same pack"
            ),
        }
    }
}

/// Anything below this is not a battery, it is an open circuit.
const PRESENT_V: f64 = 0.5;

/// How far two readings of one battery may differ: a tenth, or two volts,
/// whichever is larger. Cable drop at several amps is real, and a 12 V
/// battery cannot afford the same absolute slack as a 48 V one.
pub fn tolerance(pack_v: f64) -> f64 {
    (pack_v * 0.1).max(2.0)
}

pub fn disagrees(pack_v: f64, other_v: f64) -> bool {
    other_v > PRESENT_V && pack_v > PRESENT_V && (pack_v - other_v).abs() > tolerance(pack_v)
}

/// The first instrument that does not tell the same story as the pack.
///
/// Only call this for a pack that measures itself. A pack with no BMS is read
/// *through* these instruments, so it agrees with them by construction and
/// there is nothing here to learn.
pub fn check(pack_v: f64, instruments: &[Instrument]) -> Option<Mismatch> {
    if pack_v <= PRESENT_V {
        return None;
    }
    for i in instruments {
        let Some(v) = i.volts else { continue };
        if i.live && v <= PRESENT_V {
            return Some(Mismatch::Disconnected { who: i.who });
        }
        if disagrees(pack_v, v) {
            return Some(Mismatch::Disagree {
                who: i.who,
                theirs: v,
                pack: pack_v,
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst(who: &'static str, volts: f64, live: bool) -> Instrument {
        Instrument {
            who,
            volts: Some(volts),
            live,
        }
    }

    #[test]
    fn instruments_that_agree_say_nothing() {
        assert_eq!(
            check(51.2, &[inst("charger", 51.4, true), inst("load", 51.0, false)]),
            None
        );
    }

    #[test]
    fn cable_drop_is_not_a_fault() {
        // Three amps down a long lead: the supply reads high, the pack low.
        assert_eq!(check(51.2, &[inst("charger", 52.5, true)]), None);
    }

    #[test]
    fn a_different_battery_is_a_fault() {
        let m = check(51.2, &[inst("load", 12.6, false)]);
        assert_eq!(
            m,
            Some(Mismatch::Disagree {
                who: "load",
                theirs: 12.6,
                pack: 51.2
            })
        );
        assert!(m.unwrap().message().contains("not on the same pack"));
    }

    #[test]
    fn a_small_pack_gets_a_floor_under_the_tolerance() {
        // 10% of 12.6 V is 1.26 V, which is inside normal charging spread.
        assert!(!disagrees(12.6, 14.4));
        assert!(disagrees(12.6, 16.0));
    }

    #[test]
    fn an_instrument_delivering_into_nothing_is_disconnected() {
        assert_eq!(
            check(51.2, &[inst("charger", 0.0, true)]),
            Some(Mismatch::Disconnected { who: "charger" })
        );
    }

    #[test]
    fn an_idle_instrument_reading_nothing_is_only_idle() {
        // A supply with its output off reads its own terminals, not the
        // battery, and a load that is not sinking may read zero too.
        assert_eq!(check(51.2, &[inst("charger", 0.0, false)]), None);
    }

    #[test]
    fn a_pack_with_no_reading_is_not_something_to_cross_check() {
        assert_eq!(check(0.0, &[inst("charger", 51.2, true)]), None);
    }

    #[test]
    fn an_instrument_that_cannot_measure_is_skipped() {
        let blind = Instrument {
            who: "load",
            volts: None,
            live: true,
        };
        assert_eq!(check(51.2, &[blind]), None);
    }
}
