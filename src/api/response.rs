//! Encode engine results directly, without a second tree of JSON values.
//!
//! Call these functions on the admitted database worker: large vector results
//! must not hold up the async HTTP worker or outlive its capacity accounting.

use serde::ser::{SerializeSeq, SerializeStruct};
use serde::{Serialize, Serializer};

use super::{ApiError, ApiResultColumn};
use crate::{ExecutionResult, QueryResult, Value, Vector};

pub(super) fn encode_sql(results: &[ExecutionResult]) -> Result<Vec<u8>, ApiError> {
    #[derive(Serialize)]
    struct Response<'a> {
        results: Results<'a>,
    }
    encode(&Response {
        results: Results(results),
    })
}

pub(super) fn encode_query(result: &QueryResult) -> Result<Vec<u8>, ApiError> {
    encode(&Query(result))
}

fn encode(value: &impl Serialize) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(value)
        .map_err(|error| ApiError::internal(format!("cannot encode query response: {error}")))
}

struct Results<'a>(&'a [ExecutionResult]);

impl Serialize for Results<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for result in self.0 {
            match result {
                ExecutionResult::Query(result) => sequence.serialize_element(&Query(result))?,
                ExecutionResult::Command { tag, rows_affected } => {
                    #[derive(Serialize)]
                    struct Command<'a> {
                        r#type: &'static str,
                        tag: &'a str,
                        rows_affected: usize,
                    }
                    sequence.serialize_element(&Command {
                        r#type: "command",
                        tag,
                        rows_affected: *rows_affected,
                    })?;
                }
            }
        }
        sequence.end()
    }
}

struct Query<'a>(&'a QueryResult);

impl Serialize for Query<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let result = self.0;
        let schema = result
            .columns
            .iter()
            .zip(&result.column_types)
            .map(|(name, data_type)| ApiResultColumn {
                name: name.clone(),
                data_type: data_type.as_ref().map(ToString::to_string),
            })
            .collect::<Vec<_>>();
        let mut state = serializer.serialize_struct("Query", 6)?;
        state.serialize_field("type", "query")?;
        state.serialize_field("columns", &result.columns)?;
        state.serialize_field("schema", &schema)?;
        state.serialize_field("rows", &Rows(&result.rows))?;
        state.serialize_field("row_count", &result.row_count())?;
        state.serialize_field("rows_examined", &result.rows_examined)?;
        state.end()
    }
}

struct Rows<'a>(&'a [Vec<Value>]);

impl Serialize for Rows<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for row in self.0 {
            sequence.serialize_element(&Row(row))?;
        }
        sequence.end()
    }
}

struct Row<'a>(&'a [Value]);

impl Serialize for Row<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.len()))?;
        for value in self.0 {
            sequence.serialize_element(&Cell(value))?;
        }
        sequence.end()
    }
}

struct Cell<'a>(&'a Value);

impl Serialize for Cell<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self.0 {
            Value::Null => serializer.serialize_none(),
            Value::Integer(value) => serializer.serialize_i64(*value),
            Value::Float(value) => serializer.serialize_f64(*value),
            Value::Text(value) => serializer.serialize_str(value),
            Value::Boolean(value) => serializer.serialize_bool(*value),
            Value::Vector(vector) => VectorValues(vector).serialize(serializer),
        }
    }
}

struct VectorValues<'a>(&'a Vector);

impl Serialize for VectorValues<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut sequence = serializer.serialize_seq(Some(self.0.dimensions()))?;
        for value in self.0.as_slice() {
            // Preserve the existing API's f32-to-f64 JSON number conversion.
            sequence.serialize_element(&f64::from(*value))?;
        }
        sequence.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{ApiExecutionResult, SqlResponse};
    use crate::DataType;

    #[test]
    fn direct_encoding_preserves_the_existing_wire_format() {
        let result = QueryResult {
            columns: vec![
                "id".into(),
                "text".into(),
                "active".into(),
                "embedding".into(),
                "score".into(),
                "unknown".into(),
            ],
            column_types: vec![
                Some(DataType::Integer),
                Some(DataType::Text),
                Some(DataType::Boolean),
                Some(DataType::Vector(3)),
                Some(DataType::Float),
                None,
            ],
            rows: vec![
                vec![
                    Value::Integer(i64::MIN),
                    Value::Text("\"quoted\"\n雪".into()),
                    Value::Boolean(true),
                    Value::Vector(Vector::new(vec![0.1, -0.0, f32::MAX]).unwrap()),
                    Value::Float(f64::INFINITY),
                    Value::Null,
                ],
                vec![
                    Value::Integer(i64::MAX),
                    Value::Text(String::new()),
                    Value::Boolean(false),
                    Value::Null,
                    Value::Float(f64::NAN),
                    Value::Float(-0.0),
                ],
            ],
            rows_examined: 19,
        };
        let expected = serde_json::to_vec(&ApiExecutionResult::from(result.clone())).unwrap();
        assert_eq!(encode_query(&result).unwrap(), expected);
        let results = vec![
            ExecutionResult::Command {
                tag: "INSERT",
                rows_affected: 2,
            },
            ExecutionResult::Query(result),
        ];
        let expected = serde_json::to_vec(&SqlResponse::from(results.clone())).unwrap();
        assert_eq!(encode_sql(&results).unwrap(), expected);
        assert_eq!(encode_sql(&[]).unwrap(), br#"{"results":[]}"#);
    }
}
