//! Bind positional SQL values without interpreting data as SQL syntax.

use sqlparser::dialect::GenericDialect;
use sqlparser::tokenizer::{Token, Tokenizer};

use crate::{Error, Result, Value};

const MAX_PARAMETERS: usize = 65_535;
const MAX_BOUND_SQL_BYTES: usize = 32 * 1024 * 1024;

/// Bind `$1`, `$2`, ... placeholders to typed values.
///
/// Placeholders inside quoted strings, identifiers, or comments are untouched.
/// Parameters represent values, never table/column names or SQL fragments. Every
/// supplied value must be referenced; repeated and out-of-order references work.
/// The expanded statement is capped at 32 MiB and contains at most 65,535 inputs.
pub fn bind_parameters(sql: &str, parameters: &[Value]) -> Result<String> {
    if sql.len() > MAX_BOUND_SQL_BYTES {
        return Err(invalid(
            "bound SQL exceeds 32 MiB; split the request into batches",
        ));
    }
    if parameters.len() > MAX_PARAMETERS {
        return Err(invalid("at most 65535 SQL parameters are supported"));
    }
    let tokens = Tokenizer::new(&GenericDialect {}, sql)
        .tokenize_with_location()
        .map_err(|error| Error::Parse(error.to_string()))?;
    let mut used = vec![false; parameters.len()];
    let mut bound = String::new();
    // Token locations count Unicode scalar values, not UTF-8 bytes. Advance a
    // single cursor over the original input, preserving every non-placeholder
    // byte (including escaping, comments, and dialect-specific literal syntax).
    let mut positions = sql.char_indices();
    let (mut line, mut column) = (1, 1);
    let mut previous = 0;
    for token in tokens {
        let Token::Placeholder(name) = token.token else {
            continue;
        };
        let index = name
            .strip_prefix('$')
            .filter(|digits| !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
            .and_then(|digits| digits.parse::<usize>().ok())
            .and_then(|number| number.checked_sub(1))
            .filter(|index| *index < parameters.len())
            .ok_or_else(|| invalid("SQL parameters must use $1..$N with a supplied value"))?;
        let start = loop {
            let (offset, ch) = positions
                .next()
                .ok_or_else(|| invalid("invalid SQL parameter location"))?;
            let matches = line == token.location.line && column == token.location.column;
            if ch == '\n' {
                line += 1;
                column = 1;
            } else {
                column += 1;
            }
            if matches {
                break offset;
            }
        };
        append(&mut bound, &sql[previous..start])?;
        append(&mut bound, "(")?;
        append(&mut bound, &literal(&parameters[index])?)?;
        append(&mut bound, ")")?;
        previous = start + name.len();
        used[index] = true;
    }
    if used.iter().any(|used| !used) {
        return Err(invalid("every supplied SQL parameter must be referenced"));
    }
    append(&mut bound, &sql[previous..])?;
    Ok(bound)
}

fn append(output: &mut String, text: &str) -> Result<()> {
    if output.len().saturating_add(text.len()) > MAX_BOUND_SQL_BYTES {
        return Err(invalid(
            "bound SQL exceeds 32 MiB; split the request into batches",
        ));
    }
    output.push_str(text);
    Ok(())
}

fn literal(value: &Value) -> Result<String> {
    Ok(match value {
        Value::Null => "NULL".into(),
        // The parser represents a negative number as unary minus over a positive
        // literal. i64::MIN's magnitude is outside i64; keep both operands valid.
        Value::Integer(i64::MIN) => "-9223372036854775807 - 1".into(),
        Value::Integer(value) => value.to_string(),
        Value::Float(value) if value.is_finite() => {
            // Preserve floating point type even when the value is integral.
            format!("{value:?}")
        }
        Value::Float(_) => return Err(invalid("SQL parameters must be finite")),
        Value::Boolean(value) => if *value { "TRUE" } else { "FALSE" }.into(),
        Value::Text(value) => format!("'{}'", value.replace('\'', "''")),
        Value::Vector(value) => format!(
            "ARRAY[{value}]",
            value = value
                .as_slice()
                .iter()
                .map(|value| format!("{value:?}"))
                .collect::<Vec<_>>()
                .join(",")
        ),
    })
}

fn invalid(message: &str) -> Error {
    Error::InvalidQuery(message.into())
}
