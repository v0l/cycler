use anyhow::Result;
use clap::{Parser, Subcommand};
use cycler_core::device::{self, Device, open_discharger};
use cycler_core::{charge, pack};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "cycler", about = "Battery cycle testing")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

/// Chemistry, as a CLI argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ChemArg {
    Lifepo4,
    #[value(alias = "lipo")]
    LiIon,
    Lto,
    LeadAcid,
}

impl From<ChemArg> for cycler_core::chemistry::Chemistry {
    fn from(c: ChemArg) -> Self {
        match c {
            ChemArg::Lifepo4 => Self::LiFePo4,
            ChemArg::LiIon => Self::LiIon,
            ChemArg::Lto => Self::Lto,
            ChemArg::LeadAcid => Self::LeadAcid,
        }
    }
}

/// Load regulation mode, as a CLI argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum ModeArg {
    Cc,
    Cv,
    Cr,
    Cp,
}

impl From<ModeArg> for cycler_core::device::LoadMode {
    fn from(m: ModeArg) -> Self {
        match m {
            ModeArg::Cc => Self::Cc,
            ModeArg::Cv => Self::Cv,
            ModeArg::Cr => Self::Cr,
            ModeArg::Cp => Self::Cp,
        }
    }
}

#[derive(Subcommand)]
enum Cmd {
    /// Identify a charger or load and print one measurement.
    Probe {
        /// Device spec, e.g. owon:/dev/ttyUSB3 or dl24:
        spec: String,
    },
    /// Charge the pack under cell-level control.
    Charge {
        #[arg(long, value_enum, default_value_t = charge::Mode::Standard)]
        mode: charge::Mode,
        #[arg(long, default_value = "pylontech-console:")]
        pack: String,
        #[arg(long, default_value = "owon:")]
        charger: String,
        #[arg(long, default_value_t = 3.0)]
        max_current: f64,
        #[arg(long, default_value_t = 51.5)]
        cv: f64,
        #[arg(long, default_value_t = 3500)]
        ceiling_mv: u16,
        #[arg(long, default_value_t = 3450)]
        target_mv: u16,
        /// Termination current: absorption ends when the pack stops taking
        /// this much. C/20 of the pack is the usual figure.
        #[arg(long, default_value_t = 0.2)]
        stop_current: f64,
        #[arg(long, default_value_t = 20)]
        interval: u64,
        /// Longest absorption before it stops waiting for the stop current.
        #[arg(long, default_value_t = 6.0)]
        absorb_hours: f64,
        /// Longest balance hold, and how long a float is maintained.
        #[arg(long, default_value_t = 48.0)]
        hold_hours: f64,
        /// Stop at this state of charge, e.g. 50 for storage.
        #[arg(long)]
        stop_at_soc: Option<u8>,
        /// Chemistry of a pack with no BMS, for voltage-derived SOC.
        #[arg(long, value_enum, default_value_t = ChemArg::Lifepo4)]
        chemistry: ChemArg,
        /// Cells in series, for a pack with no BMS.
        #[arg(long, default_value_t = 15)]
        series: u16,
        /// Append every sample to this CSV.
        #[arg(long)]
        log: Option<std::path::PathBuf>,
    },
    /// Discharge at constant current (or another mode) until the first cell
    /// reaches its floor.
    Discharge {
        #[arg(long, default_value = "pylontech-console:")]
        pack: String,
        #[arg(long, default_value = "dl24:")]
        load: String,
        #[arg(long, value_enum, default_value_t = ModeArg::Cc)]
        mode: ModeArg,
        #[arg(long, default_value_t = 3.0)]
        setpoint: f64,
        #[arg(long, default_value_t = 3000)]
        floor_mv: u16,
        /// Stop at this state of charge, e.g. 50 for storage.
        #[arg(long)]
        stop_at_soc: Option<u8>,
        /// Chemistry of a pack with no BMS, for voltage-derived SOC.
        #[arg(long, value_enum, default_value_t = ChemArg::Lifepo4)]
        chemistry: ChemArg,
        /// Cells in series, for a pack with no BMS.
        #[arg(long, default_value_t = 15)]
        series: u16,
        #[arg(long, default_value_t = 5)]
        interval: u64,
        #[arg(long)]
        log: Option<std::path::PathBuf>,
    },
    /// Run a full capacity test: charge, rest, discharge counting Ah, rest.
    Cycle {
        #[arg(long, default_value = "pylontech-console:")]
        pack: String,
        #[arg(long, default_value = "owon:")]
        charger: String,
        #[arg(long, default_value = "dl24:")]
        load: String,
        #[arg(long, value_enum, default_value_t = charge::Mode::Standard)]
        charge_mode: charge::Mode,
        #[arg(long, default_value_t = 3.0)]
        max_current: f64,
        #[arg(long, default_value_t = 51.5)]
        cv: f64,
        #[arg(long, default_value_t = 3500)]
        ceiling_mv: u16,
        #[arg(long, default_value_t = 3.0)]
        discharge_a: f64,
        #[arg(long, default_value_t = 3000)]
        floor_mv: u16,
        /// Termination current: absorption ends when the pack stops taking
        /// this much. C/20 of the pack is the usual figure.
        #[arg(long, default_value_t = 0.2)]
        stop_current: f64,
        /// Chemistry of a pack with no BMS, for voltage-derived SOC.
        #[arg(long, value_enum, default_value_t = ChemArg::Lifepo4)]
        chemistry: ChemArg,
        /// Cells in series, for a pack with no BMS.
        #[arg(long, default_value_t = 15)]
        series: u16,
        #[arg(long, default_value_t = 30.0)]
        rest_min: f64,
        #[arg(long, default_value_t = 1)]
        cycles: usize,
        #[arg(long, default_value_t = 5)]
        interval: u64,
        #[arg(long)]
        log: Option<std::path::PathBuf>,
    },
    /// Drive the load by hand, to check control end to end.
    Load {
        #[arg(long, default_value = "dl24:")]
        spec: String,
        #[arg(long)]
        amps: Option<f64>,
        #[arg(long)]
        on: bool,
        #[arg(long)]
        off: bool,
        #[arg(long, default_value_t = 6)]
        samples: u32,
        /// Reset the USB device before talking to it.
        #[arg(long)]
        reset: bool,
        /// Set the load's own low-voltage cutoff, in volts.
        #[arg(long)]
        cutoff: Option<f64>,
        /// Dump the live-data payload as floats.
        #[arg(long)]
        dump: bool,
    },
    /// Talk SCPI to a serial instrument: identify it, or send one command.
    /// Read-only unless you pass a command that writes.
    Scpi {
        port: String,
        /// Command to send. Queries (ending in ?) print the reply.
        #[arg(default_value = "*IDN?")]
        command: String,
        #[arg(long, default_value_t = 9600)]
        baud: u32,
        /// Try a battery of likely queries and report which ones answer.
        #[arg(long)]
        discover: bool,
    },
    /// Work out the limits for a pack: chemistry, series and parallel in,
    /// every voltage and current out. Prints the flags to use.
    Profile {
        #[arg(long, value_enum, default_value_t = ChemArg::Lifepo4)]
        chemistry: ChemArg,
        #[arg(long, default_value_t = 15)]
        series: u16,
        #[arg(long, default_value_t = 1)]
        parallel: u16,
        #[arg(long, default_value_t = 50.0)]
        cell_ah: f64,
        /// Charge rate, as a fraction of capacity. Defaults to the
        /// chemistry's own figure.
        #[arg(long)]
        charge_c: Option<f64>,
        /// Discharge rate, which decides what the measured capacity means.
        #[arg(long)]
        discharge_c: Option<f64>,
    },
    /// List the backends and what each could open on this host.
    Devices,
    /// Zero the DL24's accumulated mAh, Wh and runtime totals.
    Reset {
        #[arg(default_value = "dl24:")]
        spec: String,
    },
    /// Poll a DL24 and print decoded counters plus the raw replies.
    Sniff {
        #[arg(default_value = "dl24:")]
        spec: String,
        #[arg(long, default_value_t = 5)]
        samples: u32,
    },
}

/// Drive a plan, printing and optionally logging every sample.
fn run_plan(
    pack: &mut dyn cycler_core::Pack,
    charger: Option<&mut dyn cycler_core::Charger>,
    load: Option<&mut dyn cycler_core::Discharger>,
    plan: cycler_core::cycle::Plan,
    interval: u64,
    log: Option<std::path::PathBuf>,
) -> Result<Vec<cycler_core::cycle::StepResult>> {
    let mut csv = match log {
        Some(p) => {
            let l = cycler_core::log::CsvLog::create(&p)?;
            println!("logging to {}", l.path().display());
            Some(l)
        }
        None => None,
    };
    let mut last_label = String::new();
    cycler_core::cycle::run(
        pack,
        charger,
        load,
        plan,
        Duration::from_secs(interval),
        |s, runner, demand| {
            if runner.current_label() != last_label {
                last_label = runner.current_label();
                println!("--- cycle {} step {}", runner.cycle() + 1, last_label);
            }
            println!(
                "{:.3} V {:+.2} A soc {:>3}% cells {}-{} spread {:>3} mV {:.1} C | {}",
                s.pack_v,
                s.current_a,
                s.soc,
                s.low_mv(),
                s.high_mv(),
                s.spread_mv(),
                s.temp_c,
                runner.note
            );
            if let Some(l) = csv.as_mut()
                && let Err(e) = l.write(
                    s,
                    &cycler_core::log::Row {
                        set_a: demand.charger_a,
                        output_on: demand.charger_on,
                        note: &runner.note,
                        load: None,
                    },
                )
            {
                eprintln!("log write: {e:#}");
            }
        },
    )
}

/// The point of the exercise: what each step did, and the capacity it measured.
fn report(results: &[cycler_core::cycle::StepResult]) {
    println!();
    for r in results {
        match r.amp_hours {
            Some(ah) => println!(
                "cycle {} {:<28} {:<14} {:.3} Ah in {:.1} h",
                r.cycle,
                r.label,
                r.outcome,
                ah,
                r.duration.as_secs_f64() / 3600.0
            ),
            None => println!(
                "cycle {} {:<28} {:<14} {:.1} h",
                r.cycle,
                r.label,
                r.outcome,
                r.duration.as_secs_f64() / 3600.0
            ),
        }
    }
    let caps: Vec<f64> = results.iter().filter_map(|r| r.amp_hours).collect();
    if !caps.is_empty() {
        let mean = caps.iter().sum::<f64>() / caps.len() as f64;
        println!("\nmeasured capacity: {mean:.2} Ah over {} discharge(s)", caps.len());
    }
}

fn main() -> Result<()> {
    // Before any device is opened: a Ctrl-C between opening a supply and
    // reaching the run loop must still leave the output off.
    cycler_core::interrupt::install();
    match Cli::parse().cmd {
        Cmd::Probe { spec } => {
            let mut dev: Box<dyn Device> = match device::open_charger(&spec) {
                Ok(c) => c as Box<dyn Device>,
                Err(_) => open_discharger(&spec)? as Box<dyn Device>,
            };
            println!("{}", dev.name());
            println!("limits: {:?}", dev.limits());
            match dev.measure() {
                Ok(s) => println!("{:.3} V  {:.3} A", s.volts, s.amps),
                Err(e) => println!("measure: {e}"),
            }
        }
        Cmd::Charge {
            mode,
            pack: pack_spec,
            charger: charger_spec,
            max_current,
            cv,
            ceiling_mv,
            target_mv,
            stop_current,
            interval,
            absorb_hours,
            hold_hours,
            stop_at_soc,
            chemistry,
            series,
            log,
        } => {
            let mut pack = pack::open_pack(&pack_spec)?;
            let profile = cycler_core::chemistry::PackProfile {
                chemistry: chemistry.into(),
                series,
                ..Default::default()
            };
            pack.set_profile(profile);
            let mut charger = device::open_charger(&charger_spec)?;
            println!("{} <- {}", pack.name(), charger.name());
            let cfg = charge::Config {
                mode,
                v_absorb: cv,
                cell_ceiling_mv: ceiling_mv,
                cell_hard_mv: ceiling_mv + 50,
                cell_target_mv: target_mv,
                v_float: cv - 0.5,
                v_recharge: cv - 2.0,
                i_term: stop_current,
                stop_at_soc,
                i_max: max_current,
                interval: Duration::from_secs(interval),
                absorb_max: Duration::from_secs_f64(absorb_hours * 3600.0),
                hold_max: Duration::from_secs_f64(hold_hours * 3600.0),
                ..charge::Config::for_profile(&profile)
            };
            let mut csv = match log {
                Some(p) => {
                    let l = cycler_core::log::CsvLog::create(&p)?;
                    println!("logging to {}", l.path().display());
                    Some(l)
                }
                None => None,
            };
            let reason = charge::run(pack.as_mut(), charger.as_mut(), cfg, |s, c| {
                if let Some(l) = csv.as_mut()
                    && let Err(e) = l.write(
                        s,
                        &cycler_core::log::Row {
                            set_a: c.set_a,
                            output_on: c.output_on,
                            note: &c.note,
                            load: None,
                        },
                    )
                {
                    eprintln!("log write: {e:#}");
                }
                println!(
                    "{:.3} V {:+.2} A soc {:>3}% cells {}-{} spread {:>3} mV (hi c{}) {:.1} C | set {:.2} A {} [{}]",
                    s.pack_v,
                    s.current_a,
                    s.soc,
                    s.low_mv(),
                    s.high_mv(),
                    s.spread_mv(),
                    s.high_cell(),
                    s.temp_c,
                    c.set_a,
                    if c.output_on { "ON" } else { "off" },
                    c.note
                );
            })?;
            println!("finished: {reason:?}");
        }
        Cmd::Discharge {
            pack: pack_spec,
            load: load_spec,
            mode,
            setpoint,
            floor_mv,
            stop_at_soc,
            chemistry,
            series,
            interval,
            log,
        } => {
            let mut pack = pack::open_pack(&pack_spec)?;
            let profile = cycler_core::chemistry::PackProfile {
                chemistry: chemistry.into(),
                series,
                ..Default::default()
            };
            pack.set_profile(profile);
            let mut load = cycler_core::open_discharger(&load_spec)?;
            println!("{} -> {}", pack.name(), load.name());
            let plan = cycler_core::cycle::Plan::discharge(cycler_core::discharge::Config {
                mode: mode.into(),
                setpoint,
                cell_floor_mv: floor_mv,
                stop_at_soc,
                interval: Duration::from_secs(interval),
                temp_min_c: profile.chemistry.discharge_min_c(),
                ..Default::default()
            });
            let results = run_plan(pack.as_mut(), None, Some(load.as_mut()), plan, interval, log)?;
            report(&results);
        }
        Cmd::Cycle {
            pack: pack_spec,
            charger: charger_spec,
            load: load_spec,
            charge_mode,
            max_current,
            cv,
            ceiling_mv,
            discharge_a,
            floor_mv,
            stop_current,
            chemistry,
            series,
            rest_min,
            cycles,
            interval,
            log,
        } => {
            let mut pack = pack::open_pack(&pack_spec)?;
            let profile = cycler_core::chemistry::PackProfile {
                chemistry: chemistry.into(),
                series,
                ..Default::default()
            };
            pack.set_profile(profile);
            let mut charger = device::open_charger(&charger_spec)?;
            let mut load = cycler_core::open_discharger(&load_spec)?;
            println!(
                "{} <- {} / -> {}",
                pack.name(),
                charger.name(),
                load.name()
            );
            let plan = cycler_core::cycle::Plan::capacity(
                charge::Config {
                    mode: charge_mode,
                    v_absorb: cv,
                    v_float: cv - 0.5,
                    v_recharge: cv - 2.0,
                    cell_ceiling_mv: ceiling_mv,
                    cell_hard_mv: ceiling_mv + 50,
                    i_max: max_current,
                    i_term: stop_current,
                    ..charge::Config::for_profile(&profile)
                },
                cycler_core::discharge::Config {
                    setpoint: discharge_a,
                    cell_floor_mv: floor_mv,
                    temp_min_c: profile.chemistry.discharge_min_c(),
                    ..Default::default()
                },
                Duration::from_secs_f64(rest_min * 60.0),
                cycles.max(1),
            );
            let results = run_plan(
                pack.as_mut(),
                Some(charger.as_mut()),
                Some(load.as_mut()),
                plan,
                interval,
                log,
            )?;
            report(&results);
        }
        Cmd::Load {
            spec,
            amps,
            on,
            off,
            samples,
            reset,
            cutoff,
            dump,
        } => {
            if reset {
                cycler_core::device::dl24::Dl24::reset_usb()?;
                println!("usb reset");
            }
            let mut load = cycler_core::open_discharger(&spec)?;
            println!("{}", load.name());
            if dump || cutoff.is_some() {
                let dl = load
                    .as_any()
                    .downcast_mut::<cycler_core::device::dl24::Dl24>()
                    .expect("dl24");
                if let Some(v) = cutoff {
                    dl.set_cutoff_volts(v as f32)?;
                    println!("cutoff {v:.2} V");
                }
                if dump && let Some(r) = dl.live_raw()? {
                    for (i, c) in r[4..62].chunks(4).enumerate() {
                        if c.len() == 4 {
                            let f = f32::from_be_bytes([c[0], c[1], c[2], c[3]]);
                            println!("  +{:<3} {:02x}{:02x}{:02x}{:02x}  {f}", i * 4, c[0], c[1], c[2], c[3]);
                        }
                    }
                }
                return Ok(());
            }
            if let Some(a) = amps {
                load.set_current(a)?;
                println!("set {a:.2} A");
            }
            if on {
                load.start()?;
                println!("start");
            }
            if off {
                load.stop()?;
                println!("stop");
            }
            for _ in 0..samples {
                let s = load.state()?;
                println!(
                    "set {:.3} | {:.2} V {:.3} A {:.1} W {:.4} Ah  load {}",
                    s.setpoint,
                    s.volts,
                    s.amps,
                    s.watts,
                    s.amp_hours,
                    if s.on { "ON" } else { "off" }
                );
                std::thread::sleep(Duration::from_millis(800));
            }
        }
        Cmd::Scpi {
            port,
            command,
            baud,
            discover,
        } => {
            let mut io = cycler_core::device::scpi::Scpi::open(&port, baud)?;
            if discover {
                // An instrument answers what it implements and ignores the
                // rest, so asking is cheaper than reading a manual we do not
                // have.
                for q in cycler_core::device::scpi::LOAD_PROBES {
                    match io.ask(q) {
                        Ok(r) if !r.is_empty() => println!("{q:<24} -> {r}"),
                        _ => println!("{q:<24} -> (no reply)"),
                    }
                }
            } else if command.trim_end().ends_with('?') {
                println!("{}", io.ask(&command)?);
            } else {
                io.send(&command)?;
                println!("sent {command}");
            }
        }
        Cmd::Profile {
            chemistry,
            series,
            parallel,
            cell_ah,
            charge_c,
            discharge_c,
        } => {
            let p = cycler_core::chemistry::PackProfile {
                chemistry: chemistry.into(),
                series,
                parallel,
                cell_ah,
                ceiling_mv: None,
            };
            let cell = p.cell();
            println!(
                "{} {}S{}P, {:.0} Ah",
                p.chemistry.label(),
                p.series,
                p.parallel,
                p.capacity_ah()
            );
            println!(
                "  cell    ceiling {} mV  float {} mV  floor {} mV  storage {} mV",
                cell.ceiling_mv, cell.float_mv, cell.floor_mv, cell.storage_mv
            );
            println!(
                "  pack    charge {:.2} V  float {:.2} V  floor {:.2} V  storage {:.2} V",
                p.charge_v(),
                p.float_v(),
                p.floor_v(),
                p.storage_v()
            );
            let charge_c = charge_c.unwrap_or_else(|| p.chemistry.default_charge_c());
            let discharge_c = discharge_c.unwrap_or_else(|| p.chemistry.default_discharge_c());
            let charge_a = p.current_at_c(charge_c);
            let discharge_a = p.current_at_c(discharge_c);
            println!("  current {charge_a:.2} A in at {charge_c}C, {discharge_a:.2} A out at {discharge_c}C");
            println!();
            println!(
                "cycler cycle --cv {:.2} --ceiling-mv {} --max-current {charge_a:.2} \\\n  --discharge-a {discharge_a:.2} --floor-mv {} --stop-current {:.2}",
                p.charge_v(),
                cell.ceiling_mv,
                cell.floor_mv,
                p.current_at_c(p.chemistry.default_termination_c())
            );
        }
        Cmd::Devices => {
            use cycler_core::device::{CHARGER_BACKENDS, LOAD_BACKENDS};
            use cycler_core::pack::PACK_BACKENDS;
            for (role, list) in [
                ("pack", PACK_BACKENDS),
                ("charger", CHARGER_BACKENDS),
                ("load", LOAD_BACKENDS),
            ] {
                for b in list {
                    println!("{role:<8} {:<10} {}", b.kind, b.label);
                    let found = b.candidates();
                    if found.is_empty() {
                        println!("           (nothing found)");
                    }
                    for c in found {
                        println!(
                            "           {} {}  {}",
                            if c.matches_ids { "*" } else { " " },
                            c.label,
                            b.spec(&c.target)
                        );
                    }
                }
            }
        }
        Cmd::Reset { spec } => {
            let (_, target) = spec.split_once(':').unwrap_or((spec.as_str(), ""));
            let mut load = device::dl24::Dl24::open(target)?;
            let before = load.counters()?;
            load.reset_counters()?;
            std::thread::sleep(Duration::from_millis(500));
            let after = load.counters()?;
            println!(
                "{}: {:.3} Ah / {:.2} Wh -> {:.3} Ah / {:.2} Wh",
                load.name(),
                before.amp_hours,
                before.watt_hours,
                after.amp_hours,
                after.watt_hours
            );
        }
        Cmd::Sniff { spec, samples } => {
            let (_, target) = spec.split_once(':').unwrap_or((spec.as_str(), ""));
            let mut load = device::dl24::Dl24::open(target)?;
            println!("{}", load.name());
            for _ in 0..samples {
                match load.counters() {
                    Ok(c) => println!(
                        "{:.3} V  {:.3} A  {:.1} W  {:.3} Ah  {:.2} Wh  {:.1} C  load {}",
                        c.volts,
                        c.amps,
                        c.watts,
                        c.amp_hours,
                        c.watt_hours,
                        c.mosfet_temp_c,
                        if c.load_on { "ON" } else { "off" }
                    ),
                    Err(e) => println!("{e}"),
                }
                std::thread::sleep(Duration::from_secs(1));
            }
        }
    }
    Ok(())
}
