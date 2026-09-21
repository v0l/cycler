use crate::device::LoadState;
use crate::pack::Snapshot;
use anyhow::Result;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

/// One logged sample: what the pack said, what the charger was told, and what
/// the load had taken so far.
#[derive(Debug, Default, Clone)]
pub struct Row<'a> {
    pub set_a: f64,
    pub output_on: bool,
    pub note: &'a str,
    pub load: Option<LoadState>,
}

pub struct CsvLog {
    out: Box<dyn Write + Send>,
    path: PathBuf,
    header: bool,
}

impl CsvLog {
    pub fn create(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir)?;
        }
        let existing = path.exists() && std::fs::metadata(&path)?.len() > 0;
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            out: Box::new(file),
            path,
            header: existing,
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn write(&mut self, s: &Snapshot, row: &Row) -> Result<()> {
        let line = format(s, row, &mut self.header, || {
            chrono::Local::now().format("%Y-%m-%dT%H:%M:%S").to_string()
        });
        self.out.write_all(line.as_bytes())?;
        self.out.flush()?;
        Ok(())
    }
}

const FIXED: &str = "ts,pack_v,current_a,soc,soh,cycles,temp_c,cell_min,cell_max,spread,hi_cell,\
set_a,output_on,load_v,load_a,load_ah,load_wh,balancing,alarms,note";

fn format(s: &Snapshot, row: &Row, header: &mut bool, now: impl Fn() -> String) -> String {
    let mut out = String::new();
    if !*header {
        out.push_str(FIXED);
        for i in 0..s.cells_mv.len() {
            out.push_str(&format!(",c{i}"));
        }
        out.push('\n');
        *header = true;
    }
    let l = row.load.unwrap_or_default();
    let opt = |v: Option<f64>| v.map(|x| format!("{x:.0}")).unwrap_or_default();
    out.push_str(&format!(
        "{},{:.3},{:.3},{},{},{},{:.1},{},{},{},{},{:.2},{},{:.2},{:.3},{:.4},{:.2},{},{},{}",
        now(),
        s.pack_v,
        s.current_a,
        s.soc,
        opt(s.soh),
        opt(s.cycles),
        s.temp_c,
        s.low_mv(),
        s.high_mv(),
        s.spread_mv(),
        s.high_cell(),
        row.set_a,
        if row.output_on { 1 } else { 0 },
        l.volts,
        l.amps,
        l.amp_hours,
        l.watt_hours,
        s.balancing
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join("|"),
        s.alarms.join("|").replace(',', ";"),
        row.note.replace(',', ";"),
    ));
    for mv in &s.cells_mv {
        out.push_str(&format!(",{mv}"));
    }
    out.push('\n');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap() -> Snapshot {
        Snapshot {
            cells_mv: vec![3331, 3400],
            balancing: vec![1],
            pack_v: 6.731,
            current_a: 1.25,
            temp_c: 24.5,
            soc: 34,
            soh: Some(87.0),
            cycles: Some(1328.0),
            alarms: Vec::new(),
            rated_ah: Some(50.0),
        }
    }

    #[test]
    fn writes_header_once_then_rows() {
        let mut header = false;
        let s = snap();
        let row = Row {
            set_a: 1.2,
            output_on: true,
            note: "ramp to 1.20 A, cap 3",
            load: None,
        };
        let first = format(&s, &row, &mut header, || "T".into());
        let mut lines = first.lines();
        assert_eq!(
            lines.next().unwrap(),
            format!("{FIXED},c0,c1").as_str()
        );
        let data = lines.next().unwrap();
        // Commas in a note would shift every later column.
        assert!(data.contains("ramp to 1.20 A; cap 3"));
        assert!(data.ends_with(",3331,3400"));
        assert_eq!(data.matches(',').count(), FIXED.matches(',').count() + 2);

        let second = format(&s, &row, &mut header, || "T".into());
        assert_eq!(second.lines().count(), 1);
    }

    #[test]
    fn records_load_totals_when_present() {
        let mut header = true;
        let row = Row {
            load: Some(LoadState {
                volts: 51.2,
                amps: 3.0,
                amp_hours: 12.345,
                watt_hours: 620.5,
                ..Default::default()
            }),
            ..Default::default()
        };
        let line = format(&snap(), &row, &mut header, || "T".into());
        assert!(line.contains("51.20,3.000,12.3450,620.50"));
    }
}
