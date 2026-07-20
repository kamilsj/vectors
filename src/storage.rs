//! Versioned binary snapshots for the in-memory catalog.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::engine::{
    rebuild_indexes, validate_row, validate_unique, Catalog, Column, DataType, HashIndex, Table,
    Value,
};
use crate::{Error, Result, Vector, MAX_VECTOR_DIMENSIONS};

const MAGIC: &[u8; 8] = b"VECTORS\0";
const FORMAT_VERSION: u32 = 2;
const MAX_TABLES: usize = 100_000;
const MAX_COLUMNS: usize = 100_000;
const MAX_INDEXES: usize = 100_000;
const MAX_ROWS: usize = 10_000_000;
const MAX_STRING_BYTES: usize = 64 * 1024 * 1024;
const FNV_OFFSET_BASIS: u64 = 0xcbf29ce484222325;
const FNV_PRIME: u64 = 0x100000001b3;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) fn save(catalog: &Catalog, path: &Path) -> Result<()> {
    let temporary = sibling_path(path, "tmp")?;
    let file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
        .map_err(|error| path_io("create temporary snapshot", &temporary, error))?;

    let result = write_catalog(file, catalog);
    if let Err(error) = result {
        let _ = fs::remove_file(&temporary);
        return Err(error);
    }

    replace_file(&temporary, path)
}

pub(crate) fn load(path: &Path) -> Result<Catalog> {
    let file = File::open(path).map_err(|error| path_io("open snapshot", path, error))?;
    let mut reader = ChecksumIo::new(BufReader::new(file));

    let mut magic = [0_u8; MAGIC.len()];
    read_exact(&mut reader, &mut magic)?;
    if &magic != MAGIC {
        return Err(corrupt("invalid file signature"));
    }
    let version = read_u32(&mut reader)?;
    if !(1..=FORMAT_VERSION).contains(&version) {
        return Err(corrupt(format!(
            "unsupported format version {version}; supported versions are 1 through {FORMAT_VERSION}"
        )));
    }

    let table_count = read_count(&mut reader, "table count", MAX_TABLES)?;
    let mut tables = std::collections::HashMap::new();
    let mut global_index_names = HashSet::new();
    for _ in 0..table_count {
        let name = read_string(&mut reader)?;
        if name.is_empty() {
            return Err(corrupt("table name is empty"));
        }
        if tables.contains_key(&name) {
            return Err(corrupt(format!("duplicate table '{name}'")));
        }

        let column_count = read_count(&mut reader, "column count", MAX_COLUMNS)?;
        if column_count == 0 {
            return Err(corrupt(format!("table '{name}' has no columns")));
        }
        let mut columns = Vec::with_capacity(column_count.min(4096));
        let mut column_names = HashSet::new();
        for _ in 0..column_count {
            let column_name = read_string(&mut reader)?;
            if column_name.is_empty() || !column_names.insert(column_name.clone()) {
                return Err(corrupt(format!(
                    "table '{name}' has an empty or duplicate column name"
                )));
            }
            let data_type = read_data_type(&mut reader)?;
            let nullable = read_bool(&mut reader, "column nullable flag")?;
            let unique = read_bool(&mut reader, "column unique flag")?;
            columns.push(Column {
                name: column_name,
                data_type,
                nullable,
                unique,
            });
        }

        let index_count = if version >= 2 {
            read_count(&mut reader, "index count", MAX_INDEXES)?
        } else {
            0
        };
        let mut indexes = std::collections::HashMap::new();
        for _ in 0..index_count {
            let index_name = read_string(&mut reader)?;
            if index_name.is_empty()
                || indexes.contains_key(&index_name)
                || !global_index_names.insert(index_name.clone())
            {
                return Err(corrupt(format!(
                    "table '{name}' has an empty or duplicate index name"
                )));
            }
            let column = read_count(&mut reader, "index column", columns.len())?;
            if column >= columns.len() {
                return Err(corrupt(format!(
                    "index '{index_name}' references a missing column"
                )));
            }
            if matches!(columns[column].data_type, DataType::Vector(_)) {
                return Err(corrupt(format!(
                    "index '{index_name}' targets a vector column"
                )));
            }
            indexes.insert(index_name, HashIndex::new(column));
        }

        let row_count = read_count(&mut reader, "row count", MAX_ROWS)?;
        let mut rows = Vec::with_capacity(row_count.min(4096));
        for _ in 0..row_count {
            let mut row = Vec::with_capacity(columns.len());
            for column in &columns {
                row.push(read_value(&mut reader, &column.data_type)?);
            }
            validate_row(&columns, &row)
                .map_err(|error| corrupt(format!("invalid row in table '{name}': {error}")))?;
            rows.push(row);
        }
        let mut table = Table {
            columns,
            rows,
            indexes,
        };
        validate_unique(&table, &[])
            .map_err(|error| corrupt(format!("invalid table '{name}': {error}")))?;
        rebuild_indexes(&mut table);
        tables.insert(name, table);
    }

    let calculated_checksum = reader.finish_hash();
    let stored_checksum = read_u64(&mut reader)?;
    if calculated_checksum != stored_checksum {
        return Err(corrupt("snapshot checksum does not match"));
    }

    let mut trailing = [0_u8; 1];
    match reader.read(&mut trailing) {
        Ok(0) => Ok(Catalog {
            tables,
            revision: 0,
        }),
        Ok(_) => Err(corrupt("trailing data after catalog")),
        Err(error) => Err(io_error("read snapshot trailer", error)),
    }
}

fn write_catalog(file: File, catalog: &Catalog) -> Result<()> {
    ensure_maximum("table count", catalog.tables.len(), MAX_TABLES)?;
    let mut writer = ChecksumIo::new(BufWriter::new(file));
    write_bytes(&mut writer, MAGIC)?;
    write_u32(&mut writer, FORMAT_VERSION)?;
    write_count(&mut writer, catalog.tables.len())?;

    // HashMap iteration order is randomized. Sorting makes equivalent snapshots
    // byte-for-byte reproducible and easier to inspect or back up incrementally.
    let mut tables = catalog.tables.iter().collect::<Vec<_>>();
    tables.sort_unstable_by_key(|(name, _)| *name);
    for (name, table) in tables {
        ensure_maximum("column count", table.columns.len(), MAX_COLUMNS)?;
        ensure_maximum("index count", table.indexes.len(), MAX_INDEXES)?;
        ensure_maximum("row count", table.rows.len(), MAX_ROWS)?;
        write_string(&mut writer, name)?;
        write_count(&mut writer, table.columns.len())?;
        for column in &table.columns {
            write_string(&mut writer, &column.name)?;
            write_data_type(&mut writer, &column.data_type)?;
            write_bool(&mut writer, column.nullable)?;
            write_bool(&mut writer, column.unique)?;
        }
        write_count(&mut writer, table.indexes.len())?;
        let mut indexes = table.indexes.iter().collect::<Vec<_>>();
        indexes.sort_unstable_by_key(|(name, _)| *name);
        for (index_name, index) in indexes {
            write_string(&mut writer, index_name)?;
            write_count(&mut writer, index.column)?;
        }
        write_count(&mut writer, table.rows.len())?;
        for row in &table.rows {
            if row.len() != table.columns.len() {
                return Err(corrupt(format!(
                    "in-memory row in table '{name}' has the wrong width"
                )));
            }
            validate_row(&table.columns, row).map_err(|error| {
                corrupt(format!("invalid in-memory row in table '{name}': {error}"))
            })?;
            for (value, column) in row.iter().zip(&table.columns) {
                write_value(&mut writer, value, &column.data_type)?;
            }
        }
    }
    let checksum = writer.finish_hash();
    write_u64(&mut writer, checksum)?;
    writer
        .flush()
        .map_err(|error| io_error("flush snapshot", error))?;
    let buffered = writer.into_inner();
    let file = buffered
        .into_inner()
        .map_err(|error| io_error("finish snapshot", error.into_error()))?;
    file.sync_all()
        .map_err(|error| io_error("synchronize snapshot", error))
}

fn write_data_type(writer: &mut impl Write, data_type: &DataType) -> Result<()> {
    match data_type {
        DataType::Integer => write_u8(writer, 1),
        DataType::Float => write_u8(writer, 2),
        DataType::Text => write_u8(writer, 3),
        DataType::Boolean => write_u8(writer, 4),
        DataType::Vector(dimensions) => {
            write_u8(writer, 5)?;
            write_count(writer, *dimensions)
        }
    }
}

fn read_data_type(reader: &mut impl Read) -> Result<DataType> {
    match read_u8(reader)? {
        1 => Ok(DataType::Integer),
        2 => Ok(DataType::Float),
        3 => Ok(DataType::Text),
        4 => Ok(DataType::Boolean),
        5 => {
            let dimensions = read_count(reader, "vector dimensions", MAX_VECTOR_DIMENSIONS)?;
            if dimensions == 0 {
                return Err(corrupt("vector dimension is zero"));
            }
            Ok(DataType::Vector(dimensions))
        }
        tag => Err(corrupt(format!("unknown data type tag {tag}"))),
    }
}

fn write_value(writer: &mut impl Write, value: &Value, data_type: &DataType) -> Result<()> {
    if matches!(value, Value::Null) {
        return write_u8(writer, 0);
    }
    write_u8(writer, 1)?;
    match (value, data_type) {
        (Value::Integer(value), DataType::Integer) => write_i64(writer, *value),
        (Value::Float(value), DataType::Float) if value.is_finite() => {
            write_u64(writer, value.to_bits())
        }
        (Value::Text(value), DataType::Text) => write_string(writer, value),
        (Value::Boolean(value), DataType::Boolean) => write_bool(writer, *value),
        (Value::Vector(value), DataType::Vector(dimensions))
            if value.dimensions() == *dimensions =>
        {
            for element in value.as_slice() {
                if !element.is_finite() {
                    return Err(corrupt("in-memory vector contains a non-finite value"));
                }
                write_u32(writer, element.to_bits())?;
            }
            Ok(())
        }
        (value, expected) => Err(corrupt(format!(
            "in-memory {} value does not match {expected}",
            value.type_name()
        ))),
    }
}

fn read_value(reader: &mut impl Read, data_type: &DataType) -> Result<Value> {
    match read_u8(reader)? {
        0 => return Ok(Value::Null),
        1 => {}
        marker => return Err(corrupt(format!("invalid NULL marker {marker}"))),
    }
    match data_type {
        DataType::Integer => Ok(Value::Integer(read_i64(reader)?)),
        DataType::Float => {
            let value = f64::from_bits(read_u64(reader)?);
            if !value.is_finite() {
                return Err(corrupt("non-finite floating-point value"));
            }
            Ok(Value::Float(value))
        }
        DataType::Text => Ok(Value::Text(read_string(reader)?)),
        DataType::Boolean => Ok(Value::Boolean(read_bool(reader, "boolean value")?)),
        DataType::Vector(dimensions) => {
            let mut values = Vec::with_capacity(*dimensions);
            for _ in 0..*dimensions {
                let value = f32::from_bits(read_u32(reader)?);
                if !value.is_finite() {
                    return Err(corrupt("vector contains a non-finite value"));
                }
                values.push(value);
            }
            Ok(Value::Vector(Vector::new(values)?))
        }
    }
}

fn replace_file(temporary: &Path, target: &Path) -> Result<()> {
    match fs::rename(temporary, target) {
        Ok(()) => return Ok(()),
        Err(first_error) if !target.exists() => {
            let _ = fs::remove_file(temporary);
            return Err(path_io("install snapshot", target, first_error));
        }
        Err(_) => {}
    }

    // Windows does not replace an existing destination with rename(). Keep the
    // old snapshot as a backup until the new name is installed successfully.
    let backup = sibling_path(target, "bak")?;
    if let Err(error) = fs::rename(target, &backup) {
        let _ = fs::remove_file(temporary);
        return Err(path_io("move previous snapshot", target, error));
    }
    if let Err(error) = fs::rename(temporary, target) {
        let _ = fs::rename(&backup, target);
        let _ = fs::remove_file(temporary);
        return Err(path_io("install replacement snapshot", target, error));
    }
    let _ = fs::remove_file(backup);
    Ok(())
}

fn sibling_path(path: &Path, role: &str) -> Result<PathBuf> {
    let name = path
        .file_name()
        .ok_or_else(|| Error::StorageIo(format!("path '{}' has no file name", path.display())))?;
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary_name = format!(
        ".{}.{}.{}.{}",
        name.to_string_lossy(),
        std::process::id(),
        sequence,
        role
    );
    Ok(path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(temporary_name))
}

fn write_string(writer: &mut impl Write, value: &str) -> Result<()> {
    if value.len() > MAX_STRING_BYTES {
        return Err(corrupt(format!(
            "string is {} bytes; maximum is {MAX_STRING_BYTES}",
            value.len()
        )));
    }
    write_count(writer, value.len())?;
    write_bytes(writer, value.as_bytes())
}

fn read_string(reader: &mut impl Read) -> Result<String> {
    let length = read_count(reader, "string length", MAX_STRING_BYTES)?;
    let mut bytes = vec![0_u8; length];
    read_exact(reader, &mut bytes)?;
    String::from_utf8(bytes).map_err(|_| corrupt("string is not valid UTF-8"))
}

fn read_count(reader: &mut impl Read, label: &str, maximum: usize) -> Result<usize> {
    let count = read_u64(reader)?;
    let count = usize::try_from(count).map_err(|_| corrupt(format!("{label} is too large")))?;
    if count > maximum {
        return Err(corrupt(format!(
            "{label} {count} exceeds maximum {maximum}"
        )));
    }
    Ok(count)
}

fn write_count(writer: &mut impl Write, value: usize) -> Result<()> {
    let value = u64::try_from(value).map_err(|_| corrupt("value does not fit snapshot format"))?;
    write_u64(writer, value)
}

fn ensure_maximum(label: &str, value: usize, maximum: usize) -> Result<()> {
    if value > maximum {
        return Err(Error::StorageIo(format!(
            "{label} {value} exceeds snapshot maximum {maximum}"
        )));
    }
    Ok(())
}

fn read_bool(reader: &mut impl Read, label: &str) -> Result<bool> {
    match read_u8(reader)? {
        0 => Ok(false),
        1 => Ok(true),
        value => Err(corrupt(format!("invalid {label}: {value}"))),
    }
}

fn write_bool(writer: &mut impl Write, value: bool) -> Result<()> {
    write_u8(writer, u8::from(value))
}

fn read_u8(reader: &mut impl Read) -> Result<u8> {
    let mut bytes = [0_u8; 1];
    read_exact(reader, &mut bytes)?;
    Ok(bytes[0])
}

fn read_u32(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0_u8; 4];
    read_exact(reader, &mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u64(reader: &mut impl Read) -> Result<u64> {
    let mut bytes = [0_u8; 8];
    read_exact(reader, &mut bytes)?;
    Ok(u64::from_le_bytes(bytes))
}

fn read_i64(reader: &mut impl Read) -> Result<i64> {
    let mut bytes = [0_u8; 8];
    read_exact(reader, &mut bytes)?;
    Ok(i64::from_le_bytes(bytes))
}

fn write_u8(writer: &mut impl Write, value: u8) -> Result<()> {
    write_bytes(writer, &[value])
}

fn write_u32(writer: &mut impl Write, value: u32) -> Result<()> {
    write_bytes(writer, &value.to_le_bytes())
}

fn write_u64(writer: &mut impl Write, value: u64) -> Result<()> {
    write_bytes(writer, &value.to_le_bytes())
}

fn write_i64(writer: &mut impl Write, value: i64) -> Result<()> {
    write_bytes(writer, &value.to_le_bytes())
}

fn read_exact(reader: &mut impl Read, buffer: &mut [u8]) -> Result<()> {
    reader.read_exact(buffer).map_err(|error| {
        if error.kind() == std::io::ErrorKind::UnexpectedEof {
            corrupt("snapshot ended unexpectedly")
        } else {
            io_error("read snapshot", error)
        }
    })
}

fn write_bytes(writer: &mut impl Write, bytes: &[u8]) -> Result<()> {
    writer
        .write_all(bytes)
        .map_err(|error| io_error("write snapshot", error))
}

fn corrupt(message: impl Into<String>) -> Error {
    Error::CorruptSnapshot(message.into())
}

fn io_error(action: &str, error: std::io::Error) -> Error {
    Error::StorageIo(format!("{action}: {error}"))
}

fn path_io(action: &str, path: &Path, error: std::io::Error) -> Error {
    Error::StorageIo(format!("{action} '{}': {error}", path.display()))
}

struct ChecksumIo<T> {
    inner: T,
    hash: u64,
    enabled: bool,
}

impl<T> ChecksumIo<T> {
    fn new(inner: T) -> Self {
        Self {
            inner,
            hash: FNV_OFFSET_BASIS,
            enabled: true,
        }
    }

    fn finish_hash(&mut self) -> u64 {
        self.enabled = false;
        self.hash
    }

    fn into_inner(self) -> T {
        self.inner
    }

    fn update(&mut self, bytes: &[u8]) {
        if self.enabled {
            for byte in bytes {
                self.hash ^= u64::from(*byte);
                self.hash = self.hash.wrapping_mul(FNV_PRIME);
            }
        }
    }
}

impl<T: Read> Read for ChecksumIo<T> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.update(&buffer[..count]);
        Ok(count)
    }
}

impl<T: Write> Write for ChecksumIo<T> {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let count = self.inner.write(buffer)?;
        self.update(&buffer[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
