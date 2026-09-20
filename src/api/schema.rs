//! Shared schema input for ordinary tables and SQL-backed document collections.

use super::*;
use std::collections::HashSet;

const MAX_IDENTIFIER_BYTES: usize = 128;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct CreateColumn {
    name: String,
    data_type: String,
    #[serde(default = "default_nullable")]
    nullable: bool,
    #[serde(default)]
    unique: bool,
}

fn default_nullable() -> bool {
    true
}

pub(super) fn decode_columns(
    columns: Vec<CreateColumn>,
    minimum: usize,
    maximum: usize,
) -> Result<Vec<Column>, ApiError> {
    if columns.len() < minimum || columns.len() > maximum {
        return Err(ApiError::bad_request(
            "invalid_columns",
            format!("a schema must contain between {minimum} and {maximum} columns"),
        ));
    }
    let mut names = HashSet::new();
    columns
        .into_iter()
        .map(|column| {
            let name = normalized_identifier(&column.name)?;
            if !names.insert(name.clone()) {
                return Err(ApiError::bad_request(
                    "duplicate_column",
                    format!("column '{name}' appears more than once"),
                ));
            }
            let data_type = parse_type(&column.data_type)?;
            if column.unique && matches!(data_type, DataType::Vector(_)) {
                return Err(ApiError::bad_request(
                    "invalid_unique_column",
                    "unique columns must have a scalar type",
                ));
            }
            Ok(Column {
                name,
                data_type,
                nullable: column.nullable,
                unique: column.unique,
            })
        })
        .collect()
}

pub(super) fn normalized_identifier(name: &str) -> Result<String, ApiError> {
    if name.trim().is_empty()
        || name.len() > MAX_IDENTIFIER_BYTES
        || name.chars().any(char::is_control)
    {
        return Err(ApiError::bad_request(
            "invalid_name",
            format!("names must contain 1–{MAX_IDENTIFIER_BYTES} bytes and no control characters"),
        ));
    }
    Ok(name.to_ascii_lowercase())
}

pub(super) fn parse_type(value: &str) -> Result<DataType, ApiError> {
    let value = value.trim().to_ascii_uppercase();
    match value.as_str() {
        "INTEGER" => return Ok(DataType::Integer),
        "DOUBLE" => return Ok(DataType::Float),
        "TEXT" => return Ok(DataType::Text),
        "BOOLEAN" => return Ok(DataType::Boolean),
        _ => {}
    }
    if let Some(dimensions) = value
        .strip_prefix("VECTOR(")
        .and_then(|value| value.strip_suffix(')'))
    {
        if let Ok(dimensions) = dimensions.trim().parse::<usize>() {
            if dimensions > 0 && dimensions <= crate::vector::MAX_VECTOR_DIMENSIONS {
                return Ok(DataType::Vector(dimensions));
            }
        }
    }
    Err(ApiError::bad_request(
        "invalid_data_type",
        "data_type must be INTEGER, DOUBLE, TEXT, BOOLEAN, or VECTOR(n), with 1–65535 dimensions",
    ))
}
