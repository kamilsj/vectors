//! Owned JSON-to-row conversion for typed ingestion.

use std::borrow::Cow;
use std::collections::HashMap;

use serde_json::{Map, Value as JsonValue};

use super::ApiError;
use crate::{Column, DataType, Error, Value, Vector, MAX_VECTOR_DIMENSIONS};

/// Map input names once, then consume values in schema order. Missing values
/// remain NULL; the storage layer enforces nullability and unique constraints.
pub(super) fn build_insert_values(
    schema: &[Column],
    rows: Vec<Map<String, JsonValue>>,
    normalize_vectors: bool,
) -> Result<Vec<Vec<Value>>, ApiError> {
    let known = schema
        .iter()
        .enumerate()
        .map(|(index, column)| (normalized_name(&column.name), index))
        .collect::<HashMap<_, _>>();
    let mut ordered = vec![JsonValue::Null; schema.len()];
    let mut seen = vec![false; schema.len()];
    let mut values = Vec::with_capacity(rows.len());
    let mut first_value_error = None;

    for row in rows {
        seen.fill(false);
        for (name, value) in row {
            let Some(&index) = known.get(normalized_name(&name).as_ref()) else {
                return Err(ApiError::bad_request(
                    "unknown_column",
                    format!("column '{name}' does not exist"),
                ));
            };
            if std::mem::replace(&mut seen[index], true) {
                return Err(ApiError::bad_request(
                    "duplicate_column",
                    format!("column '{name}' appears more than once"),
                ));
            }
            if first_value_error.is_none() {
                ordered[index] = value;
            }
        }

        // Historically every row's names were checked before converting any
        // values. Retain that error precedence without retaining a second batch
        // of JSON rows: after a value error, only validate remaining names.
        if first_value_error.is_some() {
            continue;
        }
        let row_values = schema
            .iter()
            .zip(&mut ordered)
            .map(|(column, value)| {
                json_owned_value(
                    std::mem::take(value),
                    &column.data_type,
                    &column.name,
                    normalize_vectors,
                )
            })
            .collect::<Result<Vec<_>, _>>();
        match row_values {
            Ok(row) => values.push(row),
            Err(error) => {
                first_value_error = Some(error);
                ordered.fill(JsonValue::Null);
                values.clear();
            }
        }
    }

    match first_value_error {
        Some(error) => Err(error),
        None => Ok(values),
    }
}

fn normalized_name(name: &str) -> Cow<'_, str> {
    if name.bytes().any(|byte| byte.is_ascii_uppercase()) {
        Cow::Owned(name.to_ascii_lowercase())
    } else {
        Cow::Borrowed(name)
    }
}

fn json_owned_value(
    value: JsonValue,
    data_type: &DataType,
    column: &str,
    normalize_vector: bool,
) -> Result<Value, ApiError> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let invalid = || {
        ApiError::bad_request(
            "invalid_value",
            format!("value for column '{column}' must be {data_type}"),
        )
    };
    match data_type {
        DataType::Integer => value.as_i64().map(Value::Integer).ok_or_else(invalid),
        DataType::Float => value
            .as_f64()
            .filter(|value| value.is_finite())
            .map(Value::Float)
            .ok_or_else(invalid),
        DataType::Text => match value {
            JsonValue::String(value) => Ok(Value::Text(value)),
            _ => Err(invalid()),
        },
        DataType::Boolean => value.as_bool().map(Value::Boolean).ok_or_else(invalid),
        DataType::Vector(dimensions) => {
            let JsonValue::Array(values) = value else {
                return Err(invalid());
            };
            if values.len() != *dimensions {
                return Err(ApiError::bad_request(
                    "dimension_mismatch",
                    format!(
                        "column '{column}' expects {dimensions} dimensions, received {}",
                        values.len()
                    ),
                ));
            }
            let mut values = values
                .into_iter()
                .map(|value| {
                    value
                        .as_f64()
                        .filter(|value| (*value as f32).is_finite())
                        .map(|value| value as f32)
                        .ok_or_else(invalid)
                })
                .collect::<Result<Vec<_>, _>>()?;
            if normalize_vector {
                // Match Vector::new followed by Vector::normalized, including
                // invalid dimensions, while reusing the owned f32 allocation.
                if values.is_empty() {
                    return Err(Error::InvalidVectorDimension.into());
                }
                if values.len() > MAX_VECTOR_DIMENSIONS {
                    return Err(Error::VectorDimensionLimit {
                        found: values.len(),
                        max: MAX_VECTOR_DIMENSIONS,
                    }
                    .into());
                }
                let norm = values
                    .iter()
                    .map(|value| {
                        let value = f64::from(*value);
                        value * value
                    })
                    .sum::<f64>()
                    .sqrt();
                if norm == 0.0 {
                    return Err(Error::ZeroNorm.into());
                }
                for value in &mut values {
                    *value = (f64::from(*value) / norm) as f32;
                }
            }
            Vector::new(values)
                .map(Value::Vector)
                .map_err(ApiError::from)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn column(name: &str, data_type: DataType) -> Column {
        Column {
            name: name.into(),
            data_type,
            nullable: true,
            unique: false,
        }
    }

    fn object(value: JsonValue) -> Map<String, JsonValue> {
        let JsonValue::Object(value) = value else {
            panic!("test row must be an object");
        };
        value
    }

    fn assert_error(error: ApiError, code: &str, message: &str) {
        assert_eq!(error.code, code);
        assert_eq!(error.message, message);
        assert_eq!(error.status, actix_web::http::StatusCode::BAD_REQUEST);
    }

    #[test]
    fn maps_case_insensitive_names_in_schema_order_and_resets_missing_values() {
        let schema = vec![
            column("title", DataType::Text),
            column("ID", DataType::Integer),
            column("active", DataType::Boolean),
        ];
        let values = build_insert_values(
            &schema,
            vec![
                object(json!({"id": 1, "TITLE": "first", "Active": true})),
                object(json!({"ID": 2})),
                object(json!({"id": 3, "title": null, "active": false})),
            ],
            false,
        )
        .unwrap();
        assert_eq!(
            values,
            vec![
                vec![
                    Value::Text("first".into()),
                    Value::Integer(1),
                    Value::Boolean(true)
                ],
                vec![Value::Null, Value::Integer(2), Value::Null],
                vec![Value::Null, Value::Integer(3), Value::Boolean(false)],
            ]
        );
    }

    #[test]
    fn name_validation_precedes_value_errors_across_the_whole_batch() {
        let schema = vec![column("id", DataType::Integer)];
        assert_error(
            build_insert_values(
                &schema,
                vec![object(json!({"id": "bad"})), object(json!({"UNKNOWN": 1}))],
                false,
            )
            .unwrap_err(),
            "unknown_column",
            "column 'UNKNOWN' does not exist",
        );
        assert_error(
            build_insert_values(
                &schema,
                vec![
                    object(json!({"id": "bad"})),
                    object(json!({"ID": 1, "id": 2})),
                ],
                false,
            )
            .unwrap_err(),
            "duplicate_column",
            "column 'id' appears more than once",
        );
        assert_error(
            build_insert_values(
                &schema,
                vec![
                    object(json!({"ID": 1, "id": 2})),
                    object(json!({"other": 1})),
                ],
                false,
            )
            .unwrap_err(),
            "duplicate_column",
            "column 'id' appears more than once",
        );
    }

    #[test]
    fn value_errors_keep_row_then_schema_order() {
        let schema = vec![
            column("z", DataType::Integer),
            column("a", DataType::Boolean),
        ];
        assert_error(
            build_insert_values(
                &schema,
                vec![
                    object(json!({"a": "bad", "z": "bad"})),
                    object(json!({"z": false})),
                ],
                false,
            )
            .unwrap_err(),
            "invalid_value",
            "value for column 'z' must be INTEGER",
        );
        assert_error(
            build_insert_values(
                &schema,
                vec![
                    object(json!({"a": "bad", "z": 1})),
                    object(json!({"z": false})),
                ],
                false,
            )
            .unwrap_err(),
            "invalid_value",
            "value for column 'a' must be BOOLEAN",
        );
    }

    #[test]
    fn null_and_missing_values_remain_for_storage_constraint_validation() {
        let mut required = column("id", DataType::Integer);
        required.nullable = false;
        required.unique = true;
        assert_eq!(
            build_insert_values(
                &[required],
                vec![Map::new(), object(json!({"id": null}))],
                true
            )
            .unwrap(),
            vec![vec![Value::Null], vec![Value::Null]]
        );
        assert!(build_insert_values(&[], vec![], false).unwrap().is_empty());
        assert_eq!(
            build_insert_values(&[], vec![Map::new()], false).unwrap(),
            vec![vec![]]
        );
    }

    #[test]
    fn ascii_case_folding_preserves_non_ascii_names() {
        let schema = vec![column("Étiquette", DataType::Text)];
        assert_eq!(
            build_insert_values(&schema, vec![object(json!({"ÉTIQUETTE": "yes"}))], false).unwrap(),
            vec![vec![Value::Text("yes".into())]]
        );
        assert_error(
            build_insert_values(&schema, vec![object(json!({"étiquette": "no"}))], false)
                .unwrap_err(),
            "unknown_column",
            "column 'étiquette' does not exist",
        );
    }

    #[test]
    fn text_buffer_moves_into_output_without_cloning() {
        let text = "long document body ".repeat(1_000);
        let pointer = text.as_ptr();
        let mut row = Map::new();
        row.insert("body".into(), JsonValue::String(text));
        let rows =
            build_insert_values(&[column("body", DataType::Text)], vec![row], false).unwrap();
        let Value::Text(text) = &rows[0][0] else {
            panic!("expected text");
        };
        assert_eq!(pointer, text.as_ptr());
    }

    #[test]
    fn owned_conversion_matches_borrowed_conversion_for_scalars_and_vectors() {
        let types = [
            DataType::Integer,
            DataType::Float,
            DataType::Text,
            DataType::Boolean,
            DataType::Vector(0),
            DataType::Vector(2),
            DataType::Vector(3),
        ];
        let values = [
            json!(null),
            json!(true),
            json!(false),
            json!(0),
            json!(-1),
            json!(i64::MIN),
            json!(i64::MAX),
            json!(u64::MAX),
            json!(1.0),
            json!(0.25),
            json!(1e300),
            json!("hello"),
            json!({}),
            json!([]),
            json!([3, 4]),
            json!([0, 0]),
            json!([0, -0.0]),
            json!([1e-50, 1e-50]),
            json!([1e-40, -1e-40]),
            json!([3e38, -3e38]),
            json!([1e300, 0]),
            json!([null, 1]),
            json!([true, 1]),
            json!(["1", 2]),
            json!([1, 2, 3]),
        ];
        for data_type in &types {
            for value in &values {
                for normalize in [false, true] {
                    let expected =
                        super::super::json_typed_value(value, data_type, "value", normalize);
                    let actual = json_owned_value(value.clone(), data_type, "value", normalize);
                    assert_conversion_equal(actual, expected);
                }
            }
        }
    }

    #[test]
    fn normalization_is_bit_exact_for_long_vectors_and_dimension_limits() {
        let vector = (0..1536)
            .map(|index| json!((index as f64 - 768.0) / 997.0))
            .collect();
        let value = JsonValue::Array(vector);
        assert_conversion_equal(
            json_owned_value(value.clone(), &DataType::Vector(1536), "embedding", true),
            super::super::json_typed_value(&value, &DataType::Vector(1536), "embedding", true),
        );
        let excessive = MAX_VECTOR_DIMENSIONS + 1;
        let value = JsonValue::Array(vec![json!(0); excessive]);
        for normalize in [false, true] {
            assert_conversion_equal(
                json_owned_value(
                    value.clone(),
                    &DataType::Vector(excessive),
                    "embedding",
                    normalize,
                ),
                super::super::json_typed_value(
                    &value,
                    &DataType::Vector(excessive),
                    "embedding",
                    normalize,
                ),
            );
        }
    }

    fn assert_conversion_equal(actual: Result<Value, ApiError>, expected: Result<Value, ApiError>) {
        match (actual, expected) {
            (Ok(Value::Vector(actual)), Ok(Value::Vector(expected))) => {
                assert_eq!(actual.norm().to_bits(), expected.norm().to_bits());
                assert_eq!(
                    actual
                        .as_slice()
                        .iter()
                        .map(|value| value.to_bits())
                        .collect::<Vec<_>>(),
                    expected
                        .as_slice()
                        .iter()
                        .map(|value| value.to_bits())
                        .collect::<Vec<_>>()
                );
            }
            (Ok(actual), Ok(expected)) => assert_eq!(actual, expected),
            (Err(actual), Err(expected)) => {
                assert_eq!(actual.status, expected.status);
                assert_eq!(actual.code, expected.code);
                assert_eq!(actual.message, expected.message);
            }
            (actual, expected) => {
                panic!("owned conversion {actual:?} differs from borrowed {expected:?}")
            }
        }
    }
}
