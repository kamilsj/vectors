//! Directory-backed durability built from checkpoints and an append-only WAL.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::mem::size_of;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::engine::{Catalog, InsertConflict, Value};
use crate::{storage, Error, Result, Vector, MAX_VECTOR_DIMENSIONS};

const WAL_MAGIC: &[u8; 8] = b"VECWAL\0\0";
const WAL_VERSION: u32 = 1;
const WAL_HEADER_BYTES: u64 = 12;
const MAX_WAL_RECORD_BYTES: usize = 64 * 1024 * 1024;
const MAX_WAL_ROWS: usize = 1_000_000;
const MAX_WAL_COLUMNS: usize = 100_000;
const DEFAULT_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;
const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;

pub(crate) const CHECKPOINT_FILE: &str = "vectors.vdb";
pub(crate) const WAL_FILE: &str = "vectors.wal";
const LOCK_FILE: &str = "vectors.lock";

#[derive(Debug)]
pub(crate) struct PersistentStorage {
    directory: PathBuf,
    checkpoint_path: PathBuf,
    wal: Mutex<WalFile>,
    _lock_file: File,
    checkpoint_bytes: u64,
}

#[derive(Debug)]
struct WalFile {
    file: File,
    bytes: u64,
    failed: bool,
}

#[derive(Debug)]
pub(crate) struct PreparedWalOperation {
    bytes: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct RecoveryRecord {
    pub(crate) sequence: u64,
    pub(crate) operation: WalOperation,
}

#[derive(Debug)]
pub(crate) enum WalOperation {
    Sql(String),
    InsertRows {
        table: String,
        rows: Vec<Vec<Value>>,
        conflict: InsertConflict,
    },
}

impl PersistentStorage {
    pub(crate) fn open(directory: &Path) -> Result<(Arc<Self>, Catalog, Vec<RecoveryRecord>)> {
        fs::create_dir_all(directory)
            .map_err(|error| path_io("create data directory", directory, error))?;
        let directory = fs::canonicalize(directory)
            .map_err(|error| path_io("resolve data directory", directory, error))?;
        let lock_path = directory.join(LOCK_FILE);
        let lock_file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|error| path_io("open database lock", &lock_path, error))?;
        match lock_file.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(Error::StorageBusy(directory.display().to_string()))
            }
            Err(TryLockError::Error(error)) => {
                return Err(path_io("lock data directory", &lock_path, error))
            }
        }

        let checkpoint_path = directory.join(CHECKPOINT_FILE);
        let catalog = if checkpoint_path.exists() {
            storage::load(&checkpoint_path)?
        } else {
            Catalog::default()
        };
        let wal_path = directory.join(WAL_FILE);
        let (wal, records) = open_wal(&wal_path)?;
        let storage = Arc::new(Self {
            directory,
            checkpoint_path,
            wal: Mutex::new(wal),
            _lock_file: lock_file,
            checkpoint_bytes: DEFAULT_CHECKPOINT_BYTES,
        });
        Ok((storage, catalog, records))
    }

    pub(crate) fn prepare_sql(sql: &str) -> Result<PreparedWalOperation> {
        let mut bytes = Vec::with_capacity(sql.len().saturating_add(5));
        bytes.push(1);
        write_string(&mut bytes, sql)?;
        ensure_record_size(bytes.len())?;
        Ok(PreparedWalOperation { bytes })
    }

    pub(crate) fn prepare_insert_rows(
        table: &str,
        rows: &[Vec<Value>],
        conflict: &InsertConflict,
    ) -> Result<PreparedWalOperation> {
        let mut bytes = Vec::new();
        bytes.push(2);
        write_string(&mut bytes, table)?;
        write_conflict(&mut bytes, conflict)?;
        write_count(&mut bytes, rows.len())?;
        for row in rows {
            write_count(&mut bytes, row.len())?;
            for value in row {
                write_value(&mut bytes, value)?;
            }
        }
        ensure_record_size(bytes.len())?;
        Ok(PreparedWalOperation { bytes })
    }

    pub(crate) fn append(&self, sequence: u64, operation: PreparedWalOperation) -> Result<bool> {
        let body_length = operation
            .bytes
            .len()
            .checked_add(size_of::<u64>())
            .ok_or_else(|| Error::StorageIo("WAL record size overflow".into()))?;
        ensure_record_size(body_length)?;
        let body_length = u32::try_from(body_length)
            .map_err(|_| Error::StorageIo("WAL record is too large".into()))?;
        let mut record = Vec::with_capacity(body_length as usize + 12);
        record.extend_from_slice(&body_length.to_le_bytes());
        record.extend_from_slice(&sequence.to_le_bytes());
        record.extend_from_slice(&operation.bytes);
        let checksum = checksum(&record[4..]);
        record.extend_from_slice(&checksum.to_le_bytes());

        let mut wal = self.wal.lock().map_err(|_| Error::LockPoisoned)?;
        if wal.failed {
            return Err(Error::StorageIo(
                "WAL is unavailable after an earlier write failure; reopen the database".into(),
            ));
        }
        if let Err(error) = append_record(&mut wal.file, &record) {
            wal.failed = true;
            return Err(path_io(
                "append and synchronize WAL",
                &self.directory.join(WAL_FILE),
                error,
            ));
        }
        wal.bytes = wal.bytes.saturating_add(record.len() as u64);
        Ok(wal.bytes >= self.checkpoint_bytes)
    }

    pub(crate) fn checkpoint(&self, catalog: &Catalog) -> Result<()> {
        let mut wal = self.wal.lock().map_err(|_| Error::LockPoisoned)?;
        if wal.failed {
            return Err(Error::StorageIo(
                "WAL is unavailable after an earlier write failure; reopen the database".into(),
            ));
        }
        storage::save(catalog, &self.checkpoint_path)?;
        if let Err(error) = reset_wal(&mut wal.file) {
            wal.failed = true;
            return Err(path_io(
                "reset WAL after checkpoint",
                &self.directory.join(WAL_FILE),
                error,
            ));
        }
        wal.bytes = WAL_HEADER_BYTES;
        Ok(())
    }

    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }
}

fn open_wal(path: &Path) -> Result<(WalFile, Vec<RecoveryRecord>)> {
    let mut file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .map_err(|error| path_io("open WAL", path, error))?;
    let length = file
        .metadata()
        .map_err(|error| path_io("inspect WAL", path, error))?
        .len();
    if length < WAL_HEADER_BYTES {
        reset_wal(&mut file).map_err(|error| path_io("initialize WAL", path, error))?;
        sync_parent(path)?;
        return Ok((
            WalFile {
                file,
                bytes: WAL_HEADER_BYTES,
                failed: false,
            },
            Vec::new(),
        ));
    }

    file.seek(SeekFrom::Start(0))
        .map_err(|error| path_io("seek WAL", path, error))?;
    let mut magic = [0_u8; WAL_MAGIC.len()];
    file.read_exact(&mut magic)
        .map_err(|error| path_io("read WAL header", path, error))?;
    if &magic != WAL_MAGIC {
        return Err(corrupt_wal("invalid file signature"));
    }
    let mut version = [0_u8; 4];
    file.read_exact(&mut version)
        .map_err(|error| path_io("read WAL version", path, error))?;
    let version = u32::from_le_bytes(version);
    if version != WAL_VERSION {
        return Err(corrupt_wal(format!(
            "unsupported format version {version}; expected {WAL_VERSION}"
        )));
    }

    let mut records = Vec::new();
    let mut previous_sequence = None::<u64>;
    let valid_length = loop {
        let record_start = file
            .stream_position()
            .map_err(|error| path_io("inspect WAL position", path, error))?;
        let mut length_bytes = [0_u8; 4];
        match read_complete(&mut file, &mut length_bytes)
            .map_err(|error| path_io("read WAL record length", path, error))?
        {
            ReadStatus::End => break record_start,
            ReadStatus::Incomplete => break truncate_tail(&mut file, path, record_start)?,
            ReadStatus::Complete => {}
        }
        let body_length = u32::from_le_bytes(length_bytes) as usize;
        if !(size_of::<u64>() + 1..=MAX_WAL_RECORD_BYTES).contains(&body_length) {
            return Err(corrupt_wal(format!(
                "record at byte {record_start} has invalid length {body_length}"
            )));
        }
        let mut body = vec![0_u8; body_length];
        if read_complete(&mut file, &mut body)
            .map_err(|error| path_io("read WAL record", path, error))?
            != ReadStatus::Complete
        {
            break truncate_tail(&mut file, path, record_start)?;
        }
        let mut checksum_bytes = [0_u8; 8];
        if read_complete(&mut file, &mut checksum_bytes)
            .map_err(|error| path_io("read WAL checksum", path, error))?
            != ReadStatus::Complete
        {
            break truncate_tail(&mut file, path, record_start)?;
        }
        let stored_checksum = u64::from_le_bytes(checksum_bytes);
        if checksum(&body) != stored_checksum {
            return Err(corrupt_wal(format!(
                "record at byte {record_start} failed its checksum"
            )));
        }
        let record = decode_record(&body)?;
        if let Some(previous) = previous_sequence {
            if previous.checked_add(1) != Some(record.sequence) {
                return Err(corrupt_wal(format!(
                    "record sequence {} follows {previous}",
                    record.sequence
                )));
            }
        }
        previous_sequence = Some(record.sequence);
        records.push(record);
    };
    file.seek(SeekFrom::End(0))
        .map_err(|error| path_io("seek to WAL end", path, error))?;
    Ok((
        WalFile {
            file,
            bytes: valid_length,
            failed: false,
        },
        records,
    ))
}

fn append_record(file: &mut File, record: &[u8]) -> io::Result<()> {
    file.seek(SeekFrom::End(0))?;
    file.write_all(record)?;
    file.sync_data()
}

fn reset_wal(file: &mut File) -> io::Result<()> {
    file.set_len(0)?;
    file.seek(SeekFrom::Start(0))?;
    file.write_all(WAL_MAGIC)?;
    file.write_all(&WAL_VERSION.to_le_bytes())?;
    file.sync_all()
}

fn truncate_tail(file: &mut File, path: &Path, length: u64) -> Result<u64> {
    file.set_len(length)
        .and_then(|()| file.sync_data())
        .map_err(|error| path_io("truncate incomplete WAL tail", path, error))?;
    Ok(length)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadStatus {
    Complete,
    Incomplete,
    End,
}

fn read_complete(reader: &mut impl Read, buffer: &mut [u8]) -> io::Result<ReadStatus> {
    let mut offset = 0;
    while offset < buffer.len() {
        match reader.read(&mut buffer[offset..])? {
            0 if offset == 0 => return Ok(ReadStatus::End),
            0 => return Ok(ReadStatus::Incomplete),
            count => offset += count,
        }
    }
    Ok(ReadStatus::Complete)
}

fn decode_record(body: &[u8]) -> Result<RecoveryRecord> {
    let mut decoder = Decoder::new(body);
    let sequence = decoder.u64()?;
    let operation = match decoder.u8()? {
        1 => WalOperation::Sql(decoder.string()?),
        2 => {
            let table = decoder.string()?;
            let conflict = decoder.conflict()?;
            let row_count = decoder.count("row count", MAX_WAL_ROWS)?;
            let mut rows = Vec::with_capacity(row_count.min(4096));
            for _ in 0..row_count {
                let column_count = decoder.count("column count", MAX_WAL_COLUMNS)?;
                let mut row = Vec::with_capacity(column_count.min(4096));
                for _ in 0..column_count {
                    row.push(decoder.value()?);
                }
                rows.push(row);
            }
            WalOperation::InsertRows {
                table,
                rows,
                conflict,
            }
        }
        tag => return Err(corrupt_wal(format!("unknown operation tag {tag}"))),
    };
    decoder.finish()?;
    Ok(RecoveryRecord {
        sequence,
        operation,
    })
}

fn write_conflict(bytes: &mut Vec<u8>, conflict: &InsertConflict) -> Result<()> {
    match conflict {
        InsertConflict::Fail => bytes.push(0),
        InsertConflict::DoNothing { target } => {
            bytes.push(1);
            bytes.push(u8::from(target.is_some()));
            if let Some(target) = target {
                write_string(bytes, target)?;
            }
        }
        InsertConflict::DoUpdate {
            target,
            update_columns,
        } => {
            bytes.push(2);
            write_string(bytes, target)?;
            write_count(bytes, update_columns.len())?;
            for column in update_columns {
                write_string(bytes, column)?;
            }
        }
    }
    Ok(())
}

fn write_value(bytes: &mut Vec<u8>, value: &Value) -> Result<()> {
    match value {
        Value::Null => bytes.push(0),
        Value::Integer(value) => {
            bytes.push(1);
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        Value::Float(value) if value.is_finite() => {
            bytes.push(2);
            bytes.extend_from_slice(&value.to_bits().to_le_bytes());
        }
        Value::Float(_) => return Err(Error::StorageIo("cannot log a non-finite float".into())),
        Value::Text(value) => {
            bytes.push(3);
            write_string(bytes, value)?;
        }
        Value::Boolean(value) => {
            bytes.push(4);
            bytes.push(u8::from(*value));
        }
        Value::Vector(value) => {
            bytes.push(5);
            write_count(bytes, value.dimensions())?;
            for element in value.as_slice() {
                if !element.is_finite() {
                    return Err(Error::StorageIo(
                        "cannot log a vector with a non-finite element".into(),
                    ));
                }
                bytes.extend_from_slice(&element.to_bits().to_le_bytes());
            }
        }
    }
    Ok(())
}

fn write_string(bytes: &mut Vec<u8>, value: &str) -> Result<()> {
    write_count(bytes, value.len())?;
    bytes.extend_from_slice(value.as_bytes());
    Ok(())
}

fn write_count(bytes: &mut Vec<u8>, value: usize) -> Result<()> {
    let value =
        u32::try_from(value).map_err(|_| Error::StorageIo("WAL value is too large".into()))?;
    bytes.extend_from_slice(&value.to_le_bytes());
    Ok(())
}

fn ensure_record_size(length: usize) -> Result<()> {
    if length > MAX_WAL_RECORD_BYTES {
        Err(Error::StorageIo(format!(
            "WAL record is {length} bytes; maximum is {MAX_WAL_RECORD_BYTES}"
        )))
    } else {
        Ok(())
    }
}

struct Decoder<'a> {
    bytes: &'a [u8],
    offset: usize,
}

impl<'a> Decoder<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn u32(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn u64(&mut self) -> Result<u64> {
        let bytes = self.take(8)?;
        Ok(u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]))
    }

    fn count(&mut self, label: &str, maximum: usize) -> Result<usize> {
        let value = self.u32()? as usize;
        if value > maximum {
            Err(corrupt_wal(format!(
                "{label} {value} exceeds maximum {maximum}"
            )))
        } else {
            Ok(value)
        }
    }

    fn string(&mut self) -> Result<String> {
        let length = self.count("string length", MAX_WAL_RECORD_BYTES)?;
        let bytes = self.take(length)?;
        String::from_utf8(bytes.to_vec()).map_err(|_| corrupt_wal("string is not valid UTF-8"))
    }

    fn conflict(&mut self) -> Result<InsertConflict> {
        match self.u8()? {
            0 => Ok(InsertConflict::Fail),
            1 => {
                let target = match self.u8()? {
                    0 => None,
                    1 => Some(self.string()?),
                    marker => {
                        return Err(corrupt_wal(format!(
                            "invalid conflict target marker {marker}"
                        )))
                    }
                };
                Ok(InsertConflict::DoNothing { target })
            }
            2 => {
                let target = self.string()?;
                let count = self.count("update column count", MAX_WAL_COLUMNS)?;
                let mut update_columns = Vec::with_capacity(count.min(4096));
                for _ in 0..count {
                    update_columns.push(self.string()?);
                }
                Ok(InsertConflict::DoUpdate {
                    target,
                    update_columns,
                })
            }
            tag => Err(corrupt_wal(format!("unknown conflict tag {tag}"))),
        }
    }

    fn value(&mut self) -> Result<Value> {
        match self.u8()? {
            0 => Ok(Value::Null),
            1 => Ok(Value::Integer(self.u64()? as i64)),
            2 => {
                let value = f64::from_bits(self.u64()?);
                if value.is_finite() {
                    Ok(Value::Float(value))
                } else {
                    Err(corrupt_wal("non-finite floating-point value"))
                }
            }
            3 => Ok(Value::Text(self.string()?)),
            4 => match self.u8()? {
                0 => Ok(Value::Boolean(false)),
                1 => Ok(Value::Boolean(true)),
                marker => Err(corrupt_wal(format!("invalid boolean value {marker}"))),
            },
            5 => {
                let dimensions = self.count("vector dimensions", MAX_VECTOR_DIMENSIONS)?;
                if dimensions == 0 {
                    return Err(corrupt_wal("vector dimension is zero"));
                }
                let mut elements = Vec::with_capacity(dimensions);
                for _ in 0..dimensions {
                    let bytes = self.take(4)?;
                    let value = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                    if !value.is_finite() {
                        return Err(corrupt_wal("vector contains a non-finite value"));
                    }
                    elements.push(value);
                }
                Ok(Value::Vector(Vector::new(elements)?))
            }
            tag => Err(corrupt_wal(format!("unknown value tag {tag}"))),
        }
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8]> {
        let end = self
            .offset
            .checked_add(length)
            .ok_or_else(|| corrupt_wal("record offset overflow"))?;
        let bytes = self
            .bytes
            .get(self.offset..end)
            .ok_or_else(|| corrupt_wal("record ended unexpectedly"))?;
        self.offset = end;
        Ok(bytes)
    }

    fn finish(self) -> Result<()> {
        if self.offset == self.bytes.len() {
            Ok(())
        } else {
            Err(corrupt_wal("trailing data in record"))
        }
    }
}

fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().fold(FNV_OFFSET_BASIS, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(FNV_PRIME)
    })
}

fn corrupt_wal(message: impl Into<String>) -> Error {
    Error::CorruptWal(message.into())
}

fn path_io(action: &str, path: &Path, error: io::Error) -> Error {
    Error::StorageIo(format!("{action} '{}': {error}", path.display()))
}

#[cfg(unix)]
fn sync_parent(path: &Path) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| path_io("synchronize WAL directory", parent, error))
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) -> Result<()> {
    Ok(())
}
