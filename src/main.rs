use innodb_controldata::{format_name, read_control_data, ControlData, Layout};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

const USAGE: &str = "\
Usage: innodb_controldata [OPTIONS] <DATADIR>

Reads InnoDB redo-log control data offline (MySQL 5.7.9+ / 8.x).
Run it against a stopped server or a copy of the datadir.

Options:
  --redo-dir <DIR>   Where the redo log lives, if not in DATADIR
                     (innodb_log_group_home_dir)
  --ibdata <FILE>    System tablespace file (default: DATADIR/ibdata1)
  --json             Print JSON instead of text
  -h, --help         Show this help";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let (mut datadir, mut redo_dir, mut ibdata, mut json) = (None, None, None, false);

    while let Some(a) = args.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            "--json" => json = true,
            "--redo-dir" => redo_dir = args.next().map(PathBuf::from),
            "--ibdata" => ibdata = args.next().map(PathBuf::from),
            _ if a.starts_with('-') => {
                eprintln!("unknown option: {a}\n\n{USAGE}");
                return ExitCode::from(2);
            }
            _ => datadir = Some(PathBuf::from(a)),
        }
    }
    let Some(datadir) = datadir else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };

    let redo_dir = redo_dir.unwrap_or_else(|| datadir.clone());
    let ibdata = ibdata.unwrap_or_else(|| datadir.join("ibdata1"));
    let ibdata = ibdata.exists().then_some(ibdata);

    match read_control_data(&redo_dir, ibdata.as_deref()) {
        Ok(cd) => {
            if json {
                print_json(&cd);
            } else {
                print_text(&cd);
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("innodb_controldata: {e}");
            ExitCode::FAILURE
        }
    }
}

fn rel<'a>(cd: &ControlData, p: &'a Path) -> std::borrow::Cow<'a, str> {
    p.strip_prefix(&cd.redo_dir).unwrap_or(p).to_string_lossy()
}

fn state(cd: &ControlData) -> &'static str {
    if cd.clean_shutdown() { "shut down cleanly" } else { "in production or crashed (recovery needed)" }
}

fn print_text(cd: &ControlData) {
    let layout = match cd.layout {
        Layout::Modern => "#innodb_redo/#ib_redoN (8.0.30+)",
        Layout::Legacy => "ib_logfileN (5.7.9 - 8.0.29)",
    };
    let row = |k: &str, v: String| println!("{:<38}{v}", format!("{k}:"));

    row("Redo log layout", layout.into());
    row("Redo log format", format!("{} ({})", cd.header.format, format_name(cd.header.format)));
    row("Redo log creator", cd.header.creator.clone());
    if cd.layout == Layout::Modern {
        row("Redo log UUID", cd.header.log_uuid.to_string());
        row("Redo header flags", format!("0x{:08x}", cd.header.flags));
    }
    row("Redo files", format!("{} (capacity {} bytes)", cd.files.len(), cd.redo_capacity()));
    row("Database state", state(cd).into());
    println!();
    row("Latest checkpoint LSN", cd.checkpoint.lsn.to_string());
    row("Latest checkpoint location", format!("{} @ {}", rel(cd, &cd.checkpoint_file), cd.checkpoint_file_offset));
    row("Latest checkpoint slot", format!("{} (in {})", cd.checkpoint.slot, rel(cd, &cd.checkpoint.file)));
    if cd.checkpoint.checkpoint_no != 0 {
        row("Latest checkpoint number", cd.checkpoint.checkpoint_no.to_string());
    }
    println!();
    row("Redo log end LSN", cd.redo_end_lsn.to_string());
    row("Redo log end location", format!("{} @ {}", rel(cd, &cd.redo_end_file), cd.redo_end_file_offset));
    row("Checkpoint age (bytes)", cd.checkpoint_age().to_string());
    row("Redo blocks scanned", cd.blocks_scanned.to_string());
    if let Some(l) = cd.ibdata_flushed_lsn {
        row("ibdata1 flushed LSN", l.to_string());
    }
    for w in &cd.warnings {
        eprintln!("warning: {w}");
    }
}

fn js(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn print_json(cd: &ControlData) {
    let layout = match cd.layout {
        Layout::Modern => "modern",
        Layout::Legacy => "legacy",
    };
    let opt = |v: Option<u64>| v.map_or("null".to_string(), |v| v.to_string());
    let warnings: Vec<String> = cd.warnings.iter().map(|w| js(w)).collect();
    println!(
        r#"{{
  "layout": {},
  "format": {},
  "creator": {},
  "log_uuid": {},
  "header_flags": {},
  "redo_files": {},
  "redo_capacity": {},
  "clean_shutdown": {},
  "state": {},
  "checkpoint_lsn": {},
  "checkpoint_file": {},
  "checkpoint_offset": {},
  "checkpoint_slot": {},
  "checkpoint_no": {},
  "redo_end_lsn": {},
  "redo_end_file": {},
  "redo_end_offset": {},
  "checkpoint_age": {},
  "blocks_scanned": {},
  "ibdata_flushed_lsn": {},
  "warnings": [{}]
}}"#,
        js(layout),
        cd.header.format,
        js(&cd.header.creator),
        cd.header.log_uuid,
        cd.header.flags,
        cd.files.len(),
        cd.redo_capacity(),
        cd.clean_shutdown(),
        js(state(cd)),
        cd.checkpoint.lsn,
        js(&rel(cd, &cd.checkpoint_file)),
        cd.checkpoint_file_offset,
        cd.checkpoint.slot,
        cd.checkpoint.checkpoint_no,
        cd.redo_end_lsn,
        js(&rel(cd, &cd.redo_end_file)),
        cd.redo_end_file_offset,
        cd.checkpoint_age(),
        cd.blocks_scanned,
        opt(cd.ibdata_flushed_lsn),
        warnings.join(", ")
    );
}
