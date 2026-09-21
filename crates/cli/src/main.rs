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
        #[arg(long, value_enum, default_value_t = charge::Mode::Bulk)]
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
        #[arg(long, default_value_t = 20)]
        interval: u64,
        #[arg(long, default_value_t = 48.0)]
        hold_hours: f64,
        /// Stop at this state of charge, e.g. 50 for storage.
        #[arg(long)]
        stop_at_soc: Option<u8>,
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
        #[arg(long, value_enum, default_value_t = charge::Mode::Auto)]
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
        /// C-rate for the suggested currents.
        #[arg(long, default_value_t = 0.2)]
        c_rate: f64,
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
            interval,
            hold_hours,
            stop_at_soc,
            log,
        } => {
            let mut pack = pack::open_pack(&pack_spec)?;
            let mut charger = device::open_charger(&charger_spec)?;
            println!("{} <- {}", pack.name(), charger.name());
            let cfg = charge::Config {
                mode,
                pack_cv: cv,
                cell_ceiling_mv: ceiling_mv,
                cell_hard_mv: ceiling_mv + 50,
                cell_resume_mv: ceiling_mv.saturating_sub(40),
                cell_target_mv: target_mv,
                float_v: cv - 0.5,
                stop_at_soc,
                i_max: max_current,
                interval: Duration::from_secs(interval),
                hold_max: Duration::from_secs_f64(hold_hours * 3600.0),
                ..Default::default()
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
            interval,
            log,
        } => {
            let mut pack = pack::open_pack(&pack_spec)?;
            let mut load = cycler_core::open_discharger(&load_spec)?;
            println!("{} -> {}", pack.name(), load.name());
            let plan = cycler_core::cycle::Plan::discharge(cycler_core::discharge::Config {
                mode: mode.into(),
                setpoint,
                cell_floor_mv: floor_mv,
                stop_at_soc,
                interval: Duration::from_secs(interval),
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
            rest_min,
            cycles,
            interval,
            log,
        } => {
            let mut pack = pack::open_pack(&pack_spec)?;
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
                    pack_cv: cv,
                    float_v: cv - 0.5,
                    cell_ceiling_mv: ceiling_mv,
                    cell_hard_mv: ceiling_mv + 50,
                    cell_resume_mv: ceiling_mv.saturating_sub(40),
                    i_max: max_current,
                    ..Default::default()
                },
                cycler_core::discharge::Config {
                    setpoint: discharge_a,
                    cell_floor_mv: floor_mv,
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
            c_rate,
        } => {
            let p = cycler_core::chemistry::PackProfile {
                chemistry: chemistry.into(),
                series,
                parallel,
                cell_ah,
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
            let i = p.current_at_c(c_rate);
            println!("  current {i:.2} A at {c_rate}C");
            println!();
            println!(
                "cycler cycle --cv {:.2} --ceiling-mv {} --max-current {i:.2} \\\n  --discharge-a {i:.2} --floor-mv {}",
                p.charge_v(),
                cell.ceiling_mv,
                cell.floor_mv
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
