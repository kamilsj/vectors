//! Named relationships reuse ordinary scalar columns, SQL joins, and indexes.

use super::schema::normalized_identifier;
use super::*;
use std::collections::HashMap;

const RELATIONSHIPS_TABLE: &str = "_vectors_relationships";
const MAX_RELATIONSHIPS: usize = 256;
const FIELDS: [&str; 6] = [
    "name",
    "source_table",
    "source_column",
    "target_table",
    "target_column",
    "data_type",
];

pub(super) fn configure(config: &mut web::ServiceConfig) {
    config
        .service(
            web::resource("/relationships")
                .route(web::get().to(list))
                .route(web::post().to(create)),
        )
        .route("/relationships/{name}", web::delete().to(remove));
}

#[derive(Debug, Serialize)]
struct Relationship {
    name: String,
    source_table: String,
    source_column: String,
    target_table: String,
    target_column: String,
    data_type: String,
    valid: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

#[derive(Serialize)]
struct Relationships {
    revision: u64,
    relationships: Vec<Relationship>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateRelationship {
    name: String,
    source_table: String,
    source_column: String,
    target_table: String,
    target_column: String,
    expected_revision: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RemoveRelationship {
    expected_revision: u64,
}

fn registry_schema() -> Vec<Column> {
    FIELDS
        .iter()
        .map(|name| Column {
            name: (*name).into(),
            data_type: DataType::Text,
            nullable: false,
            unique: *name == "name",
        })
        .collect()
}

fn read_registry(database: &Database) -> Result<Vec<Relationship>, ApiError> {
    let page = match database.table_page(RELATIONSHIPS_TABLE, 0, MAX_RELATIONSHIPS + 1) {
        Ok(page) => page,
        Err(Error::TableNotFound(_)) => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    if page.columns != registry_schema() || page.total_rows > MAX_RELATIONSHIPS {
        return Err(ApiError::bad_request("invalid_relationship_catalog", "the relationship catalog schema or capacity changed; repair _vectors_relationships using SQL"));
    }
    let mut relationships = page
        .rows
        .into_iter()
        .map(|row| {
            let mut text = row.into_iter().map(|value| match value {
                Value::Text(value) => Ok(value),
                _ => Err(ApiError::bad_request(
                    "invalid_relationship_catalog",
                    "relationship definitions must contain text",
                )),
            });
            let mut next = || {
                text.next()
                    .ok_or_else(|| ApiError::internal("incomplete relationship definition"))?
            };
            Ok(Relationship {
                name: next()?,
                source_table: next()?,
                source_column: next()?,
                target_table: next()?,
                target_column: next()?,
                data_type: next()?,
                valid: true,
                error: None,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    relationships.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(relationships)
}

fn endpoint_type<'a>(
    schema: &'a [Column],
    table: &str,
    name: &str,
) -> Result<&'a DataType, ApiError> {
    let column = schema
        .iter()
        .find(|column| column.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            ApiError::bad_request(
                "invalid_relationship_column",
                format!("column '{table}.{name}' does not exist"),
            )
        })?;
    if matches!(column.data_type, DataType::Vector(_)) {
        return Err(ApiError::bad_request("invalid_relationship_column", "relationships use matching TEXT, INTEGER, DOUBLE, or BOOLEAN fields; vector similarity uses Connections"));
    }
    Ok(&column.data_type)
}

fn check_revision(database: &Database, expected: u64) -> Result<(), ApiError> {
    let actual = database.revision()?;
    if actual != expected {
        return Err(Error::RevisionConflict { expected, actual }.into());
    }
    Ok(())
}

fn list_relationships(database: &Database) -> Result<Relationships, ApiError> {
    let revision = database.revision()?;
    let mut relationships = read_registry(database)?;
    let mut schemas = HashMap::new();
    for relationship in &mut relationships {
        for table in [&relationship.source_table, &relationship.target_table] {
            schemas
                .entry(table.clone())
                .or_insert_with(|| database.schema(table));
        }
        let validation = (|| {
            let source = schemas[&relationship.source_table]
                .as_ref()
                .map_err(|error| error.to_string())?;
            let target = schemas[&relationship.target_table]
                .as_ref()
                .map_err(|error| error.to_string())?;
            let source = endpoint_type(
                source,
                &relationship.source_table,
                &relationship.source_column,
            )
            .map_err(|error| error.message)?;
            let target = endpoint_type(
                target,
                &relationship.target_table,
                &relationship.target_column,
            )
            .map_err(|error| error.message)?;
            if source != target || source.to_string() != relationship.data_type {
                return Err("relationship field types changed; recreate this relationship".into());
            }
            Ok::<_, String>(())
        })();
        if let Err(error) = validation {
            relationship.valid = false;
            relationship.error = Some(error);
        }
    }
    check_revision(database, revision)?;
    Ok(Relationships {
        revision,
        relationships,
    })
}

async fn list(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
) -> Result<web::Json<Relationships>, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let database = database.get_ref().clone();
    Ok(web::Json(
        run_database_task(limiter.as_ref(), move || list_relationships(&database)).await?,
    ))
}

fn create_relationship(
    database: &Database,
    input: CreateRelationship,
) -> Result<Relationships, ApiError> {
    check_revision(database, input.expected_revision)?;
    let name = normalized_identifier(&input.name)?;
    if name.len() > 48
        || !name.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(ApiError::bad_request(
            "invalid_relationship_name",
            "use 1–48 lowercase letters, digits, or underscores, starting with a letter",
        ));
    }
    let source_table = normalized_identifier(&input.source_table)?;
    let target_table = normalized_identifier(&input.target_table)?;
    let source_column = normalized_identifier(&input.source_column)?;
    let target_column = normalized_identifier(&input.target_column)?;
    if source_table == RELATIONSHIPS_TABLE || target_table == RELATIONSHIPS_TABLE {
        return Err(ApiError::bad_request(
            "invalid_relationship_table",
            "choose a data table or document collection",
        ));
    }
    let existing = read_registry(database)?;
    if existing.len() >= MAX_RELATIONSHIPS {
        return Err(ApiError::bad_request(
            "relationship_capacity",
            "at most 256 relationship definitions are supported",
        ));
    }
    if existing
        .iter()
        .any(|relationship| relationship.name.eq_ignore_ascii_case(&name))
    {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "relationship_exists",
            message: format!("relationship '{name}' already exists"),
        });
    }
    if existing.iter().any(|relationship| {
        relationship
            .source_table
            .eq_ignore_ascii_case(&source_table)
            && relationship
                .source_column
                .eq_ignore_ascii_case(&source_column)
            && relationship
                .target_table
                .eq_ignore_ascii_case(&target_table)
            && relationship
                .target_column
                .eq_ignore_ascii_case(&target_column)
    }) {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: "relationship_exists",
            message: "a relationship already connects these fields".into(),
        });
    }
    let registry_exists = match database.schema(RELATIONSHIPS_TABLE) {
        Ok(_) => true,
        Err(Error::TableNotFound(_)) => false,
        Err(error) => return Err(error.into()),
    };
    let mut changes = 1_u64 + u64::from(!registry_exists);
    let source_schema = database.schema(&source_table)?;
    let target_schema = database.schema(&target_table)?;
    let source_type = endpoint_type(&source_schema, &source_table, &source_column)?;
    let target_type = endpoint_type(&target_schema, &target_table, &target_column)?;
    if source_type != target_type {
        return Err(ApiError::bad_request(
            "relationship_type_mismatch",
            "linked fields must have the same scalar type",
        ));
    }
    let definitions = FIELDS
        .iter()
        .map(|field| {
            format!(
                "{} TEXT NOT NULL{}",
                quote_identifier(field),
                if *field == "name" { " UNIQUE" } else { "" }
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let mut sql = format!(
        "CREATE TABLE IF NOT EXISTS {} ({definitions});",
        quote_identifier(RELATIONSHIPS_TABLE)
    );
    let data_type = source_type.to_string();
    let values = [
        &name,
        &source_table,
        &source_column,
        &target_table,
        &target_column,
        &data_type,
    ]
    .into_iter()
    .map(|value| {
        json_literal(
            &JsonValue::String(value.clone()),
            &DataType::Text,
            "relationship",
            false,
        )
    })
    .collect::<Result<Vec<_>, _>>()?
    .join(",");
    sql.push_str(&format!(
        "INSERT INTO {} VALUES ({values});",
        quote_identifier(RELATIONSHIPS_TABLE)
    ));
    // Reuse indexes already maintained by a collection, prior relationship, or SQL.
    for (side, table, column) in [
        ("source", &source_table, &source_column),
        ("target", &target_table, &target_column),
    ] {
        if !database
            .indexes(table)?
            .iter()
            .any(|index| index.column.eq_ignore_ascii_case(column))
            && !(side == "target" && source_table == target_table && source_column == target_column)
        {
            changes += 1;
            sql.push_str(&format!(
                "CREATE INDEX {} ON {} USING HASH ({});",
                quote_identifier(&format!("_vectors_relation_{name}_{side}")),
                quote_identifier(table),
                quote_identifier(column)
            ));
        }
    }
    let increment = if database.data_directory().is_some() {
        1
    } else {
        changes
    };
    let revision = input
        .expected_revision
        .checked_add(increment)
        .ok_or_else(|| ApiError::internal("catalog revision exhausted"))?;
    database.execute_if_revision(&sql, input.expected_revision)?;
    // Return the commit's revision rather than a later unrelated catalog revision.
    Ok(Relationships {
        revision,
        relationships: vec![Relationship {
            name,
            source_table,
            source_column,
            target_table,
            target_column,
            data_type,
            valid: true,
            error: None,
        }],
    })
}

async fn create(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    input: web::Json<CreateRelationship>,
) -> Result<web::Json<JsonValue>, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let database = database.get_ref().clone();
    let input = input.into_inner();
    let result = run_database_task(limiter.as_ref(), move || {
        create_relationship(&database, input)
    })
    .await?;
    Ok(web::Json(
        serde_json::json!({ "revision": result.revision, "relationship": result.relationships.into_iter().next() }),
    ))
}

async fn remove(
    request: HttpRequest,
    security: Option<web::Data<ApiSecurity>>,
    limiter: Option<web::Data<DatabaseTaskLimiter>>,
    database: web::Data<Database>,
    name: web::Path<String>,
    input: web::Json<RemoveRelationship>,
) -> Result<web::Json<JsonValue>, ApiError> {
    authorize(&request, security.as_ref().map(|value| value.get_ref()))?;
    let database = database.get_ref().clone();
    let expected = input.expected_revision;
    let requested_name = normalized_identifier(&name)?;
    run_database_task(limiter.as_ref(), move || {
        check_revision(&database, expected)?;
        let matching = read_registry(&database)?
            .into_iter()
            .filter(|relationship| relationship.name.eq_ignore_ascii_case(&requested_name))
            .collect::<Vec<_>>();
        let name = match matching.as_slice() {
            [relationship] => relationship.name.clone(),
            [] => {
                return Err(ApiError {
                    status: StatusCode::NOT_FOUND,
                    code: "relationship_not_found",
                    message: format!("relationship '{requested_name}' does not exist"),
                })
            }
            _ => return Err(ApiError::bad_request(
                "invalid_relationship_catalog",
                "relationship names differ only by case; repair _vectors_relationships using SQL",
            )),
        };
        let revision = expected
            .checked_add(1)
            .ok_or_else(|| ApiError::internal("catalog revision exhausted"))?;
        let literal = json_literal(
            &JsonValue::String(name.clone()),
            &DataType::Text,
            "name",
            false,
        )?;
        database.execute_if_revision(
            &format!(
                "DELETE FROM {} WHERE name = {literal}",
                quote_identifier(RELATIONSHIPS_TABLE)
            ),
            expected,
        )?;
        Ok(web::Json(
            serde_json::json!({ "revision": revision, "name": name }),
        ))
    })
    .await
}

#[cfg(test)]
#[path = "relationship_tests.rs"]
mod tests;
