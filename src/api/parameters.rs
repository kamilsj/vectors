use super::*;

pub(super) fn bind(request: SqlRequest) -> Result<String, ApiError> {
    let Some(parameters) = request.parameters else {
        return Ok(request.sql);
    };
    let parameters = parameters
        .into_iter()
        .map(value)
        .collect::<Result<Vec<_>, _>>()?;
    crate::bind_parameters(&request.sql, &parameters)
        .map_err(|error| ApiError::bad_request("invalid_parameters", error.to_string()))
}

fn value(value: JsonValue) -> Result<Value, ApiError> {
    let invalid = || {
        ApiError::bad_request("invalid_parameters", "parameters must be null, booleans, signed 64-bit integers, finite numbers, strings, or nonempty finite numeric vectors")
    };
    Ok(match value {
        JsonValue::Null => Value::Null,
        JsonValue::Bool(value) => Value::Boolean(value),
        JsonValue::String(value) => Value::Text(value),
        JsonValue::Number(value) if value.is_i64() => Value::Integer(value.as_i64().unwrap()),
        JsonValue::Number(value) if value.is_f64() => Value::Float(
            value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(invalid)?,
        ),
        JsonValue::Array(values) => {
            if values.len() > crate::MAX_VECTOR_DIMENSIONS {
                return Err(invalid());
            }
            let values = values
                .into_iter()
                .map(|value| {
                    value
                        .as_f64()
                        .map(|value| value as f32)
                        .filter(|value| value.is_finite())
                        .ok_or_else(invalid)
                })
                .collect::<Result<Vec<_>, _>>()?;
            Value::Vector(Vector::new(values).map_err(|_| invalid())?)
        }
        _ => return Err(invalid()),
    })
}
