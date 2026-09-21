//! What the battery is, expressed once, so every limit can be derived from it
//! rather than typed in four places.
//!
//! A pack is a chemistry, a number of cells in series, and a number of strings
//! in parallel. Series sets every voltage limit; parallel sets the capacity
//! and therefore the sensible currents. With a BMS the series count is read
//! from the cells it reports; without one it has to be told.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Chemistry {
    /// Lithium iron phosphate. Flat curve, low ceiling, the usual storage pack.
    LiFePo4,
    /// Lithium NMC/LCO, the 3.7 V nominal laptop and EV chemistry. A LiPo
    /// is the same cell in a pouch, so it belongs here too.
    LiIon,
    /// Lithium titanate. Low voltage, very tolerant.
    Lto,
    /// Flooded or AGM lead-acid, per 2 V cell.
    LeadAcid,
}

/// Per-cell voltages in millivolts, which is how every limit in `cycler` is
/// ultimately expressed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellLimits {
    pub nominal_mv: u16,
    /// Charge ceiling: where a charge tapers.
    pub ceiling_mv: u16,
    /// Absorb/float hold voltage, below the ceiling.
    pub float_mv: u16,
    /// Discharge floor: where a discharge stops.
    pub floor_mv: u16,
    /// Resting voltage for long-term storage.
    pub storage_mv: u16,
    /// Balance target near the top of charge.
    pub balance_mv: u16,
}

impl Chemistry {
    pub const ALL: [Chemistry; 4] = [
        Chemistry::LiFePo4,
        Chemistry::LiIon,
        Chemistry::Lto,
        Chemistry::LeadAcid,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Chemistry::LiFePo4 => "LiFePO4",
            Chemistry::LiIon => "Li-ion / LiPo",
            Chemistry::Lto => "LTO",
            Chemistry::LeadAcid => "Lead-acid",
        }
    }

    /// Conservative limits: a cycle test should not be the thing that ages a
    /// pack, so these sit inside the datasheet maxima rather than on them.
    pub fn cell(self) -> CellLimits {
        match self {
            Chemistry::LiFePo4 => CellLimits {
                nominal_mv: 3200,
                ceiling_mv: 3500,
                float_mv: 3400,
                floor_mv: 3000,
                storage_mv: 3300,
                balance_mv: 3450,
            },
            Chemistry::LiIon => CellLimits {
                nominal_mv: 3700,
                ceiling_mv: 4150,
                float_mv: 4100,
                floor_mv: 3000,
                storage_mv: 3800,
                balance_mv: 4100,
            },
            Chemistry::Lto => CellLimits {
                nominal_mv: 2300,
                ceiling_mv: 2750,
                float_mv: 2700,
                floor_mv: 1800,
                storage_mv: 2300,
                balance_mv: 2700,
            },
            Chemistry::LeadAcid => CellLimits {
                // Per 2 V cell: 14.4 V absorb and 13.6 V float on a 6-cell
                // battery, 10.8 V cut-off.
                nominal_mv: 2000,
                ceiling_mv: 2400,
                float_mv: 2267,
                floor_mv: 1800,
                storage_mv: 2133,
                balance_mv: 2400,
            },
        }
    }

    /// The rate to charge at, as a fraction of capacity. Filling a pack is
    /// the half that ages it, so these sit well under what the cells allow.
    pub fn default_charge_c(self) -> f64 {
        match self {
            Chemistry::LiFePo4 => 0.2,
            Chemistry::LiIon => 0.2,
            // Built for it: an LTO cell takes several C without complaint.
            Chemistry::Lto => 0.5,
            // Above C/10 a lead-acid battery gasses and heats rather than
            // charging, and the absorption stage gets longer, not shorter.
            Chemistry::LeadAcid => 0.1,
        }
    }

    /// The rate to discharge at. This one sets what the capacity number
    /// means: a pack measured fast reads low, and lead-acid dramatically so,
    /// which is why its rating is quoted at the twenty hour rate.
    pub fn default_discharge_c(self) -> f64 {
        match self {
            Chemistry::LiFePo4 => 0.5,
            Chemistry::LiIon => 0.5,
            Chemistry::Lto => 1.0,
            Chemistry::LeadAcid => 0.05,
        }
    }

    /// The coldest a pack may be charged at, in Celsius.
    ///
    /// Below freezing, lithium plates metallic lithium on the anode instead
    /// of intercalating. It is permanent, it is cumulative, and it ends in an
    /// internal short. The cell will happily take the current while it
    /// happens, so nothing but this check stops it.
    pub fn charge_min_c(self) -> f64 {
        match self {
            Chemistry::LiFePo4 | Chemistry::LiIon => 0.0,
            // LTO's anode sits well above lithium plating potential, which is
            // the whole reason it exists.
            Chemistry::Lto => -20.0,
            // Lead-acid charges cold, but a frozen electrolyte does not, and
            // a flat battery freezes around -10 C.
            Chemistry::LeadAcid => -10.0,
        }
    }

    /// The coldest a pack may be discharged at. Colder than it may be
    /// charged: taking current out of a cold cell costs capacity and little
    /// else.
    pub fn discharge_min_c(self) -> f64 {
        match self {
            Chemistry::LiFePo4 | Chemistry::LiIon => -20.0,
            Chemistry::Lto => -30.0,
            Chemistry::LeadAcid => -20.0,
        }
    }

    /// Where absorption ends, as a fraction of capacity. Lithium is done at
    /// C/20; a lead-acid battery keeps taking a small current long after it
    /// is full, so its tail is quoted nearer 2% of capacity.
    pub fn default_termination_c(self) -> f64 {
        match self {
            Chemistry::LeadAcid => 0.02,
            _ => 0.05,
        }
    }

    /// Open-circuit voltage against state of charge, per cell, as millivolts
    /// at 0, 10, 20 ... 100%. Coarse on purpose: these are bench figures at
    /// room temperature, and the flat chemistries cannot do better.
    pub fn ocv_curve(self) -> [u16; 11] {
        match self {
            // Famously flat: 20% and 80% differ by about 40 mV, which is why
            // a voltage-derived SOC on LiFePO4 is a rough guide and nothing
            // more.
            Chemistry::LiFePo4 => [
                2500, 3000, 3200, 3250, 3270, 3290, 3300, 3310, 3330, 3350, 3450,
            ],
            Chemistry::LiIon => [
                3000, 3400, 3550, 3620, 3690, 3760, 3840, 3930, 4020, 4110, 4200,
            ],
            Chemistry::Lto => [
                1800, 2050, 2150, 2200, 2230, 2260, 2290, 2330, 2400, 2500, 2700,
            ],
            // The standard rested table, per 2 V cell: 12.70 V full and
            // 11.40 V empty on a 6-cell battery.
            Chemistry::LeadAcid => [
                1900, 1930, 1958, 1983, 2010, 2033, 2053, 2070, 2083, 2103, 2117,
            ],
        }
    }

    /// State of charge from a resting cell voltage, by interpolating the
    /// curve. Only meaningful at rest: current through the pack's internal
    /// resistance shifts the terminal voltage either way.
    pub fn soc_from_cell_mv(self, mv: u16) -> f64 {
        let curve = self.ocv_curve();
        if mv <= curve[0] {
            return 0.0;
        }
        if mv >= curve[10] {
            return 100.0;
        }
        for i in 1..curve.len() {
            if mv <= curve[i] {
                let (lo, hi) = (curve[i - 1] as f64, curve[i] as f64);
                let step = (mv as f64 - lo) / (hi - lo);
                return ((i - 1) as f64 + step) * 10.0;
            }
        }
        100.0
    }

    /// The chemistry a resting cell voltage belongs to. Only a suggestion: a
    /// BMS reports cells, never what they are made of, and a flat LiFePO4 cell
    /// and a mid-charge lead-acid cell read alike.
    pub fn from_cell_mv(mv: u16) -> Option<Self> {
        match mv {
            1600..=2100 => Some(Chemistry::LeadAcid),
            2150..=2850 => Some(Chemistry::Lto),
            2900..=3550 => Some(Chemistry::LiFePo4),
            3560..=4250 => Some(Chemistry::LiIon),
            _ => None,
        }
    }

    /// Series count that best explains a measured pack voltage, for a first
    /// guess when there is no BMS to ask.
    pub fn series_from_voltage(self, volts: f64) -> u16 {
        let nominal = self.cell().nominal_mv as f64 / 1000.0;
        ((volts / nominal).round() as u16).max(1)
    }
}

/// A pack: chemistry, series, parallel, and the capacity of one string.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PackProfile {
    pub chemistry: Chemistry,
    pub series: u16,
    pub parallel: u16,
    /// Amp-hours of a single cell, so capacity is `cell_ah * parallel`.
    pub cell_ah: f64,
    /// The ceiling this pack is actually charged to, when it is not the
    /// chemistry's own. Charging a Li-ion cell to 4.15 V instead of 4.20 V
    /// is a choice about its life, and once made it is what full means here.
    #[serde(default)]
    pub ceiling_mv: Option<u16>,
}

impl Default for PackProfile {
    fn default() -> Self {
        Self {
            chemistry: Chemistry::LiFePo4,
            series: 15,
            parallel: 1,
            cell_ah: 50.0,
            ceiling_mv: None,
        }
    }
}

impl PackProfile {
    pub fn cell(&self) -> CellLimits {
        let mut c = self.chemistry.cell();
        if let Some(mv) = self.ceiling_mv {
            c.ceiling_mv = mv;
        }
        c
    }

    /// Record the charge setpoint in per-cell terms, so everything derived
    /// from "full" follows the voltage the pack is really charged to.
    pub fn set_charge_v(&mut self, volts: f64) {
        let mv = (volts / self.series.max(1) as f64 * 1000.0).round();
        self.ceiling_mv = (mv > 0.0).then_some(mv.clamp(0.0, 65_535.0) as u16);
    }

    pub fn capacity_ah(&self) -> f64 {
        self.cell_ah * self.parallel.max(1) as f64
    }

    fn pack_v(&self, cell_mv: u16) -> f64 {
        self.series.max(1) as f64 * cell_mv as f64 / 1000.0
    }

    /// Charge setpoint: the series string at its per-cell ceiling.
    pub fn charge_v(&self) -> f64 {
        self.pack_v(self.cell().ceiling_mv)
    }

    pub fn float_v(&self) -> f64 {
        self.pack_v(self.cell().float_mv)
    }

    pub fn floor_v(&self) -> f64 {
        self.pack_v(self.cell().floor_mv)
    }

    pub fn storage_v(&self) -> f64 {
        self.pack_v(self.cell().storage_mv)
    }

    /// A current at the given C-rate, e.g. `0.2` for a gentle test.
    pub fn current_at_c(&self, c: f64) -> f64 {
        (self.capacity_ah() * c).max(0.1)
    }

    /// Estimated state of charge from terminal voltage. A guess, and worth
    /// treating as one: it is only honest at rest.
    ///
    /// Full means the ceiling this profile charges to, not the ceiling the
    /// cell chemistry could take. A Li-ion cell charged to 4.15 V for the
    /// sake of its life is about 94% of its rated capacity, and a gauge that
    /// never passes 94 makes every control reading it useless: stopping at
    /// 100% would never fire, and stopping at 50% would land somewhere else
    /// than half way up the charge that was asked for.
    pub fn soc_from_pack_v(&self, volts: f64) -> f64 {
        let per_cell = volts / self.series.max(1) as f64 * 1000.0;
        let now = self
            .chemistry
            .soc_from_cell_mv(per_cell.clamp(0.0, 65_535.0) as u16);
        let full = self.chemistry.soc_from_cell_mv(self.cell().ceiling_mv);
        if full <= 0.0 {
            return now;
        }
        (now / full * 100.0).clamp(0.0, 100.0)
    }

    /// Adopt the series count a BMS is reporting. It knows better than a
    /// setting does.
    pub fn observe_cells(&mut self, cells: usize) {
        if cells > 0 {
            self.series = cells as u16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_15s_lifepo4_pack_matches_the_bench_setup() {
        let p = PackProfile::default();
        assert!((p.charge_v() - 52.5).abs() < 1e-9);
        assert!((p.float_v() - 51.0).abs() < 1e-9);
        assert!((p.floor_v() - 45.0).abs() < 1e-9);
        assert!((p.storage_v() - 49.5).abs() < 1e-9);
        assert_eq!(p.capacity_ah(), 50.0);
    }

    #[test]
    fn lithium_may_not_be_charged_below_freezing() {
        assert!((Chemistry::LiFePo4.charge_min_c() - 0.0).abs() < 1e-9);
        assert!((Chemistry::LiIon.charge_min_c() - 0.0).abs() < 1e-9);
        // LTO is the exception: its anode does not plate.
        assert!(Chemistry::Lto.charge_min_c() < -10.0);
    }

    #[test]
    fn a_12v_lead_acid_battery_is_six_cells() {
        let p = PackProfile {
            chemistry: Chemistry::LeadAcid,
            series: 6,
            parallel: 1,
            cell_ah: 100.0,

            ceiling_mv: None,
        };
        assert!((p.charge_v() - 14.4).abs() < 1e-9);
        assert!((p.float_v() - 13.602).abs() < 1e-3);
        assert!((p.floor_v() - 10.8).abs() < 1e-9);
    }

    #[test]
    fn the_test_rates_follow_the_chemistry() {
        // C/10 in and C/20 out on lead-acid, because that is the rate its
        // capacity is quoted at.
        assert!((Chemistry::LeadAcid.default_charge_c() - 0.1).abs() < 1e-9);
        assert!((Chemistry::LeadAcid.default_discharge_c() - 0.05).abs() < 1e-9);
        // Lithium is the other way round: it charges gently and discharges
        // at whatever the test wants.
        for c in [Chemistry::LiFePo4, Chemistry::LiIon, Chemistry::Lto] {
            assert!(c.default_discharge_c() > c.default_charge_c(), "{c:?}");
        }
        for c in Chemistry::ALL {
            // Every chemistry tolerates more cold coming out than going in.
            assert!(c.discharge_min_c() < c.charge_min_c(), "{c:?}");
            assert!(c.default_charge_c() > 0.0 && c.default_charge_c() <= 1.0);
            assert!(c.default_discharge_c() > 0.0 && c.default_discharge_c() <= 2.0);
            // Terminating has to be a small fraction of charging, or the
            // charge ends before the pack is full.
            assert!(c.default_termination_c() < c.default_charge_c() / 2.0, "{c:?}");
        }
    }

    #[test]
    fn parallel_strings_scale_capacity_and_current() {
        let p = PackProfile {
            parallel: 4,
            cell_ah: 25.0,
            ..Default::default()
        };
        assert_eq!(p.capacity_ah(), 100.0);
        assert!((p.current_at_c(0.2) - 20.0).abs() < 1e-9);
        // Voltage is a series-only property.
        assert!((p.charge_v() - 52.5).abs() < 1e-9);
    }

    #[test]
    fn series_is_guessed_from_a_resting_voltage() {
        assert_eq!(Chemistry::LiFePo4.series_from_voltage(51.2), 16);
        assert_eq!(Chemistry::LeadAcid.series_from_voltage(12.6), 6);
        assert_eq!(Chemistry::LiIon.series_from_voltage(44.4), 12);
    }

    #[test]
    fn soc_is_interpolated_from_the_curve() {
        let li = Chemistry::LiIon;
        assert_eq!(li.soc_from_cell_mv(2000), 0.0);
        assert_eq!(li.soc_from_cell_mv(4300), 100.0);
        assert!((li.soc_from_cell_mv(3760) - 50.0).abs() < 1e-9);
        // Half way between the 50% and 60% points.
        assert!((li.soc_from_cell_mv(3800) - 55.0).abs() < 1.0);
    }

    #[test]
    fn soc_is_full_at_the_ceiling_the_profile_charges_to() {
        let p = PackProfile {
            chemistry: Chemistry::LiIon,
            series: 4,
            ..Default::default()
        };
        // 4.15 V per cell is this profile's ceiling, so it is 100% of the
        // charge asked for even though the cell would take 4.20 V.
        assert_eq!(p.soc_from_pack_v(16.6).round(), 100.0);
        assert!(p.soc_from_pack_v(17.0) <= 100.0);
        // Half way up the curve to that ceiling, not half way to 4.20 V.
        let half = p.soc_from_pack_v(4.0 * 3.76);
        assert!((half - 53.0).abs() < 2.0, "{half}");
    }

    #[test]
    fn a_12v_lead_acid_battery_reads_its_usual_numbers() {
        let p = PackProfile {
            chemistry: Chemistry::LeadAcid,
            series: 6,
            ..Default::default()
        };
        // 12.7 V rested is full, 12.0 V is a third, 11.4 V is flat.
        assert!(p.soc_from_pack_v(12.75) > 95.0);
        assert!((p.soc_from_pack_v(12.0) - 35.0).abs() < 10.0);
        assert!(p.soc_from_pack_v(11.35) < 2.0);
    }

    #[test]
    fn a_flat_lifepo4_pack_still_orders_correctly() {
        let p = PackProfile::default();
        let low = p.soc_from_pack_v(15.0 * 3.20);
        let mid = p.soc_from_pack_v(15.0 * 3.29);
        let high = p.soc_from_pack_v(15.0 * 3.34);
        assert!(low < mid && mid < high, "{low} {mid} {high}");
    }

    #[test]
    fn a_cell_voltage_suggests_a_chemistry() {
        assert_eq!(Chemistry::from_cell_mv(3320), Some(Chemistry::LiFePo4));
        assert_eq!(Chemistry::from_cell_mv(3950), Some(Chemistry::LiIon));
        assert_eq!(Chemistry::from_cell_mv(2300), Some(Chemistry::Lto));
        assert_eq!(Chemistry::from_cell_mv(2050), Some(Chemistry::LeadAcid));
        assert_eq!(Chemistry::from_cell_mv(900), None);
    }

    #[test]
    fn a_reporting_bms_overrides_the_setting() {
        let mut p = PackProfile::default();
        p.observe_cells(16);
        assert_eq!(p.series, 16);
        assert!((p.charge_v() - 56.0).abs() < 1e-9);
        p.observe_cells(0);
        assert_eq!(p.series, 16);
    }
}
