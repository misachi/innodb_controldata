//! Offline reader for InnoDB redo-log control data (a `pg_controldata` analogue).
//!
//! Supports two on-disk layouts:
//! * MySQL 8.0.30+: `<datadir>/#innodb_redo/#ib_redoN` (one file per LSN range)
//! * MySQL 5.7.9 .. 8.0.29: `<datadir>/ib_logfile0..N` (circular group)
//!
//! All multi-byte integers in redo files are big-endian (mach_read_from_N).

use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub const BLOCK_SIZE: u64 = 512; // OS_FILE_LOG_BLOCK_SIZE
pub const FILE_HDR_SIZE: u64 = 2048; // LOG_FILE_HDR_SIZE
const BLOCK_HDR_SIZE: u64 = 12; // LOG_BLOCK_HDR_SIZE
const BLOCK_CHECKSUM: usize = 508; // LOG_BLOCK_CHECKSUM (trailer)
const CHECKPOINT_SLOTS: [u64; 2] = [512, 1536]; // LOG_CHECKPOINT_1 / _2

// Header block (block 0 of each redo file)
const HDR_FORMAT: usize = 0; // LOG_HEADER_FORMAT
const HDR_UUID: usize = 4; // LOG_HEADER_LOG_UUID (8.0.30+), pad before
const HDR_START_LSN: usize = 8; // LOG_HEADER_START_LSN
const HDR_CREATOR: usize = 16; // LOG_HEADER_CREATOR (32 bytes)
const HDR_FLAGS: usize = 48; // LOG_HEADER_FLAGS (8.0.30+)

// Checkpoint block
const CP_NO: usize = 0; // LOG_CHECKPOINT_NO (5.7 only)
const CP_LSN: usize = 8; // LOG_CHECKPOINT_LSN
const CP_OFFSET: usize = 16; // LOG_CHECKPOINT_OFFSET (pre-8.0.30)

// Data block header
const BLK_HDR_NO: usize = 0; // LOG_BLOCK_HDR_NO
const BLK_DATA_LEN: usize = 4; // LOG_BLOCK_HDR_DATA_LEN

// ibdata1 page 0
const FIL_PAGE_FILE_FLUSH_LSN: u64 = 26;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout {
    /// 8.0.30+ `#innodb_redo/#ib_redoN`
    Modern,
    /// 5.7.9 – 8.0.29 `ib_logfileN`
    Legacy,
}

#[derive(Debug, Clone)]
pub struct RedoFile {
    pub path: PathBuf,
    pub id: u64,
    pub size: u64,
    pub start_lsn: u64,
}

#[derive(Debug, Clone)]
pub struct LogHeader {
    pub format: u32,
    pub log_uuid: u32,
    pub start_lsn: u64,
    pub creator: String,
    pub flags: u32,
    pub checksum_ok: bool,
}

#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub file: PathBuf,
    pub slot: u8,
    pub checkpoint_no: u64,
    pub lsn: u64,
    /// Group offset stored in the block (pre-8.0.30 only).
    pub offset: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct ControlData {
    pub redo_dir: PathBuf,
    pub layout: Layout,
    pub header: LogHeader,
    pub files: Vec<RedoFile>,
    pub checkpoint: Checkpoint,
    /// Byte position of the checkpoint inside its redo file.
    pub checkpoint_file: PathBuf,
    pub checkpoint_file_offset: u64,
    /// LSN at which the redo log ends (current/flushed redo position).
    pub redo_end_lsn: u64,
    pub redo_end_file: PathBuf,
    pub redo_end_file_offset: u64,
    pub blocks_scanned: u64,
    pub ibdata_flushed_lsn: Option<u64>,
    pub warnings: Vec<String>,
}

impl ControlData {
    pub fn checkpoint_age(&self) -> u64 {
        self.redo_end_lsn.saturating_sub(self.checkpoint.lsn)
    }
    pub fn clean_shutdown(&self) -> bool {
        self.redo_end_lsn == self.checkpoint.lsn
    }
    pub fn redo_capacity(&self) -> u64 {
        self.files.iter().map(|f| f.size - FILE_HDR_SIZE).sum()
    }
}


fn be16(b: &[u8], o: usize) -> u16 {
    u16::from_be_bytes(b[o..o + 2].try_into().unwrap())
}
fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes(b[o..o + 4].try_into().unwrap())
}
fn be64(b: &[u8], o: usize) -> u64 {
    u64::from_be_bytes(b[o..o + 8].try_into().unwrap())
}

const CRC32C_TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0x82F6_3B78 } else { c >> 1 };
            k += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// CRC-32C (Castagnoli), as used by InnoDB's `ut_crc32`.
pub fn crc32c(data: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in data {
        crc = CRC32C_TABLE[((crc ^ b as u32) & 0xFF) as usize] ^ (crc >> 8);
    }
    !crc
}

pub fn block_checksum_ok(block: &[u8]) -> bool {
    crc32c(&block[..BLOCK_CHECKSUM]) == be32(block, BLOCK_CHECKSUM)
}

/// log_block_convert_lsn_to_no()
pub fn lsn_to_block_no(lsn: u64) -> u32 {
    (((lsn / BLOCK_SIZE) & 0x3FFF_FFFF) + 1) as u32
}

fn read_block(path: &Path, offset: u64) -> io::Result<[u8; BLOCK_SIZE as usize]> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = [0u8; BLOCK_SIZE as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

pub fn format_name(format: u32) -> &'static str {
    match format {
        0 => "pre-5.7.9 (unsupported)",
        1 => "5.7.9+",
        2 => "8.0.1",
        3 => "8.0.3+",
        4 => "8.0.19+",
        5 => "8.0.28+",
        6 => "8.0.30+",
        _ => "unknown",
    }
}


fn find_files(redo_dir: &Path) -> io::Result<(Layout, Vec<RedoFile>)> {
    let modern_dir = redo_dir.join("#innodb_redo");
    let mut files = Vec::new();

    if modern_dir.is_dir() {
        for e in fs::read_dir(&modern_dir)? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            // skip spare files (#ib_redoN_tmp)
            if let Some(id) = name.strip_prefix("#ib_redo").and_then(|s| s.parse::<u64>().ok()) {
                files.push(RedoFile { path: e.path(), id, size: e.metadata()?.len(), start_lsn: 0 });
            }
        }
        if !files.is_empty() {
            files.sort_by_key(|f| f.id);
            return Ok((Layout::Modern, files));
        }
    }

    for id in 0.. {
        let p = redo_dir.join(format!("ib_logfile{id}"));
        match fs::metadata(&p) {
            Ok(m) => files.push(RedoFile { path: p, id, size: m.len(), start_lsn: 0 }),
            Err(_) => break,
        }
    }
    if files.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no #innodb_redo/#ib_redoN or ib_logfile0 found in {}", redo_dir.display()),
        ));
    }
    Ok((Layout::Legacy, files))
}

fn read_header(path: &Path) -> io::Result<LogHeader> {
    let b = read_block(path, 0)?;
    let creator = String::from_utf8_lossy(&b[HDR_CREATOR..HDR_CREATOR + 32])
        .trim_end_matches('\0')
        .trim()
        .to_string();
    Ok(LogHeader {
        format: be32(&b, HDR_FORMAT),
        log_uuid: be32(&b, HDR_UUID),
        start_lsn: be64(&b, HDR_START_LSN),
        creator,
        flags: be32(&b, HDR_FLAGS),
        checksum_ok: block_checksum_ok(&b),
    })
}

fn read_checkpoints(path: &Path, layout: Layout, warnings: &mut Vec<String>) -> Vec<Checkpoint> {
    let mut out = Vec::new();
    for (i, &off) in CHECKPOINT_SLOTS.iter().enumerate() {
        let Ok(b) = read_block(path, off) else { continue };
        if !block_checksum_ok(&b) {
            // empty slots are normal in 8.0.30+ (only some files carry checkpoints)
            if b.iter().any(|&x| x != 0) {
                warnings.push(format!("checkpoint slot {} in {} has a bad checksum", i + 1, path.display()));
            }
            continue;
        }
        let lsn = be64(&b, CP_LSN);
        if lsn == 0 {
            continue;
        }
        out.push(Checkpoint {
            file: path.to_path_buf(),
            slot: i as u8 + 1,
            checkpoint_no: be64(&b, CP_NO),
            lsn,
            offset: (layout == Layout::Legacy).then(|| be64(&b, CP_OFFSET)),
        });
    }
    out
}


/// Maps LSNs to (file, byte offset) for a given layout.
struct Locator<'a> {
    layout: Layout,
    files: &'a [RedoFile],
    // legacy only: a known (lsn, group offset) pair, taken from the checkpoint
    anchor_lsn: u64,
    anchor_offset: u64,
}

impl Locator<'_> {
    fn locate(&self, lsn: u64) -> Option<(&Path, u64)> {
        match self.layout {
            Layout::Modern => self.files.iter().find_map(|f| {
                let end = f.start_lsn + (f.size - FILE_HDR_SIZE);
                (f.start_lsn <= lsn && lsn < end)
                    .then(|| (f.path.as_path(), FILE_HDR_SIZE + (lsn - f.start_lsn)))
            }),
            Layout::Legacy => {
                // log_files_real_offset_for_lsn(): the group is one circular
                // space of n * (file_size - header) data bytes.
                let fs = self.files[0].size;
                let per_file = fs - FILE_HDR_SIZE;
                let capacity = per_file * self.files.len() as u64;
                let to_data = |go: u64| (go / fs) * per_file + (go % fs) - FILE_HDR_SIZE;
                let anchor = to_data(self.anchor_offset);
                let delta = (lsn as i128 - self.anchor_lsn as i128).rem_euclid(capacity as i128) as u64;
                let d = (anchor + delta) % capacity;
                let file = &self.files[(d / per_file) as usize];
                Some((file.path.as_path(), FILE_HDR_SIZE + d % per_file))
            }
        }
    }
}

/// Walks redo blocks forward from `from_lsn` until the log ends. Returns
/// (end_lsn, blocks_scanned). Mirrors what recovery does to find the tail.
fn scan_to_end(loc: &Locator, from_lsn: u64, max_bytes: u64) -> (u64, u64) {
    let mut block_lsn = from_lsn / BLOCK_SIZE * BLOCK_SIZE;
    let mut end = from_lsn;
    let mut blocks = 0;

    while block_lsn - from_lsn / BLOCK_SIZE * BLOCK_SIZE < max_bytes {
        let Some((path, off)) = loc.locate(block_lsn) else { break };
        let Ok(b) = read_block(path, off) else { break };
        if !block_checksum_ok(&b) || be32(&b, BLK_HDR_NO) & 0x7FFF_FFFF != lsn_to_block_no(block_lsn) {
            break;
        }
        let data_len = (be16(&b, BLK_DATA_LEN) & 0x7FFF) as u64;
        if !(BLOCK_HDR_SIZE..=BLOCK_SIZE).contains(&data_len) {
            break;
        }
        blocks += 1;
        if data_len < BLOCK_SIZE {
            end = block_lsn + data_len;
            break;
        }
        // full block: next writable LSN is just past the next block's header
        end = block_lsn + BLOCK_SIZE + BLOCK_HDR_SIZE;
        block_lsn += BLOCK_SIZE;
    }
    (end.max(from_lsn), blocks)
}

pub fn read_ibdata_flushed_lsn(path: &Path) -> io::Result<u64> {
    let mut f = File::open(path)?;
    f.seek(SeekFrom::Start(FIL_PAGE_FILE_FLUSH_LSN))?;
    let mut b = [0u8; 8];
    f.read_exact(&mut b)?;
    Ok(u64::from_be_bytes(b))
}

/// Reads control data. `redo_dir` is the datadir (or innodb_log_group_home_dir);
/// `ibdata` optionally points at the system tablespace's first file.
pub fn read_control_data(redo_dir: &Path, ibdata: Option<&Path>) -> io::Result<ControlData> {
    let mut warnings = Vec::new();
    let (layout, mut files) = find_files(redo_dir)?;

    let header = read_header(&files[0].path)?;
    if !header.checksum_ok {
        warnings.push(format!("header block of {} has a bad checksum", files[0].path.display()));
    }
    if header.format == 0 {
        warnings.push("redo format 0 (MySQL < 5.7.9) uses a different layout; values are likely wrong".into());
    }

    let mut checkpoints = Vec::new();
    match layout {
        Layout::Modern => {
            for f in files.iter_mut() {
                f.start_lsn = read_header(&f.path)?.start_lsn;
                checkpoints.extend(read_checkpoints(&f.path, layout, &mut warnings));
            }
        }
        Layout::Legacy => {
            let size = files[0].size;
            if files.iter().any(|f| f.size != size) {
                warnings.push("ib_logfile* files differ in size".into());
            }
            checkpoints = read_checkpoints(&files[0].path, layout, &mut warnings);
        }
    }

    // the newest valid checkpoint wins (log_files_find_and_analyze_checkpoint)
    let checkpoint = checkpoints
        .into_iter()
        .max_by_key(|c| (c.lsn, c.checkpoint_no))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no valid checkpoint found"))?;

    let loc = Locator {
        layout,
        files: &files,
        anchor_lsn: checkpoint.lsn,
        anchor_offset: checkpoint.offset.unwrap_or(0),
    };
    if layout == Layout::Legacy {
        let off = checkpoint.offset.unwrap();
        if off % BLOCK_SIZE != checkpoint.lsn % BLOCK_SIZE || off % files[0].size < FILE_HDR_SIZE {
            warnings.push(format!("checkpoint offset {off} looks inconsistent with its LSN"));
        }
    }

    let (cp_path, cp_off) = loc
        .locate(checkpoint.lsn)
        .map(|(p, o)| (p.to_path_buf(), o))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "checkpoint LSN not inside any redo file"))?;

    let capacity: u64 = files.iter().map(|f| f.size - FILE_HDR_SIZE).sum();
    let (redo_end_lsn, blocks_scanned) = scan_to_end(&loc, checkpoint.lsn, capacity);
    let (end_path, end_off) = loc
        .locate(redo_end_lsn)
        .map(|(p, o)| (p.to_path_buf(), o))
        .unwrap_or_else(|| (cp_path.clone(), cp_off));

    let ibdata_flushed_lsn = match ibdata {
        Some(p) => match read_ibdata_flushed_lsn(p) {
            Ok(v) => Some(v),
            Err(e) => {
                warnings.push(format!("could not read {}: {e}", p.display()));
                None
            }
        },
        None => None,
    };

    Ok(ControlData {
        redo_dir: redo_dir.to_path_buf(),
        layout,
        header,
        files,
        checkpoint,
        checkpoint_file: cp_path,
        checkpoint_file_offset: cp_off,
        redo_end_lsn,
        redo_end_file: end_path,
        redo_end_file_offset: end_off,
        blocks_scanned,
        ibdata_flushed_lsn,
        warnings,
    })
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn seal(b: &mut [u8; 512]) {
        let c = crc32c(&b[..BLOCK_CHECKSUM]);
        b[BLOCK_CHECKSUM..].copy_from_slice(&c.to_be_bytes());
    }

    fn header_block(format: u32, start_lsn: u64) -> [u8; 512] {
        let mut b = [0u8; 512];
        b[HDR_FORMAT..4].copy_from_slice(&format.to_be_bytes());
        b[HDR_START_LSN..16].copy_from_slice(&start_lsn.to_be_bytes());
        b[HDR_CREATOR..HDR_CREATOR + 12].copy_from_slice(b"MySQL 8.0.36");
        seal(&mut b);
        b
    }

    fn cp_block(lsn: u64, offset: u64) -> [u8; 512] {
        let mut b = [0u8; 512];
        b[CP_LSN..16].copy_from_slice(&lsn.to_be_bytes());
        b[CP_OFFSET..24].copy_from_slice(&offset.to_be_bytes());
        seal(&mut b);
        b
    }

    fn data_block(lsn: u64, data_len: u16) -> [u8; 512] {
        let mut b = [0u8; 512];
        b[0..4].copy_from_slice(&lsn_to_block_no(lsn).to_be_bytes());
        b[4..6].copy_from_slice(&data_len.to_be_bytes());
        seal(&mut b);
        b
    }

    fn write_file(path: &Path, blocks: &[[u8; 512]]) {
        let mut f = File::create(path).unwrap();
        for b in blocks {
            f.write_all(b).unwrap();
        }
    }

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("innodb_cd_{name}_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn crc32c_known_vector() {
        assert_eq!(crc32c(b"123456789"), 0xE306_9283);
    }

    #[test]
    fn modern_layout() {
        let d = tmpdir("modern");
        let rd = d.join("#innodb_redo");
        fs::create_dir(&rd).unwrap();
        let z = [0u8; 512];
        // file 7 covers LSN 8192..10240, file 8 covers 10240..12288
        write_file(
            &rd.join("#ib_redo7"),
            &[header_block(6, 8192), cp_block(8192 + 700, 0), z, z,
              data_block(8192, 512), data_block(8704, 512), data_block(9216, 512), data_block(9728, 512)],
        );
        write_file(
            &rd.join("#ib_redo8"),
            &[header_block(6, 10240), z, z, z, data_block(10240, 200), z, z, z],
        );
        write_file(&rd.join("#ib_redo9_tmp"), &[z; 8]); // spare, must be ignored

        let cd = read_control_data(&d, None).unwrap();
        assert_eq!(cd.layout, Layout::Modern);
        assert_eq!(cd.files.len(), 2);
        assert_eq!(cd.checkpoint.lsn, 8892);
        assert_eq!(cd.checkpoint_file_offset, 2048 + 700);
        assert_eq!(cd.redo_end_lsn, 10240 + 200);
        assert!(cd.redo_end_file.ends_with("#ib_redo8"));
        assert_eq!(cd.redo_end_file_offset, 2048 + 200);
        assert_eq!(cd.blocks_scanned, 4);
        assert!(!cd.clean_shutdown());
    }

    #[test]
    fn legacy_layout_crosses_files() {
        let d = tmpdir("legacy");
        let z = [0u8; 512];
        // 2 files x (2048 hdr + 2 data blocks); group data starts at LSN 8192
        // checkpoint at LSN 8704+20 => group offset 2048+512+20
        write_file(
            &d.join("ib_logfile0"),
            &[header_block(1, 8192), cp_block(8724, 2580), z, cp_block(8000, 2048),
              data_block(8192, 512), data_block(8704, 512)],
        );
        write_file(&d.join("ib_logfile1"), &[z, z, z, z, data_block(9216, 100), z]);
        let ib = d.join("ibdata1");
        let mut page = vec![0u8; 64];
        page[26..34].copy_from_slice(&8724u64.to_be_bytes());
        fs::write(&ib, page).unwrap();

        let cd = read_control_data(&d, Some(&ib)).unwrap();
        assert_eq!(cd.layout, Layout::Legacy);
        assert_eq!(cd.checkpoint.lsn, 8724);
        assert_eq!(cd.checkpoint.slot, 1);
        assert_eq!(cd.redo_end_lsn, 9316);
        assert!(cd.redo_end_file.ends_with("ib_logfile1"));
        assert_eq!(cd.redo_end_file_offset, 2048 + 100);
        assert_eq!(cd.ibdata_flushed_lsn, Some(8724));
    }

    #[test]
    fn clean_shutdown_detected() {
        let d = tmpdir("clean");
        let rd = d.join("#innodb_redo");
        fs::create_dir(&rd).unwrap();
        let z = [0u8; 512];
        write_file(&rd.join("#ib_redo1"), &[header_block(6, 8192), cp_block(8192 + 40, 0), z, z, data_block(8192, 40), z]);
        let cd = read_control_data(&d, None).unwrap();
        assert_eq!(cd.redo_end_lsn, 8232);
        assert!(cd.clean_shutdown());
    }
}
