//! Minimal embedded hybrid-search example.

use vectors::{Database, ExecutionResult};

fn main() -> vectors::Result<()> {
    let database = Database::new();
    database.execute(
        "CREATE TABLE documents (
            id INTEGER PRIMARY KEY,
            title TEXT NOT NULL,
            category TEXT,
            embedding VECTOR(3)
        );
        CREATE INDEX documents_category_idx
            ON documents USING HASH (category);",
    )?;
    database.execute(
        "INSERT INTO documents VALUES
            (1, 'Rust ownership', 'engineering', ARRAY[0.95, 0.05, 0.00]),
            (2, 'SQL query planning', 'engineering', ARRAY[0.80, 0.20, 0.05]),
            (3, 'Garden notes', 'personal', ARRAY[0.05, 0.10, 0.95]);",
    )?;

    let results = database.execute(
        "SELECT id, title,
                cosine_distance(embedding, ARRAY[1, 0, 0]) AS distance
         FROM documents
         WHERE category = 'engineering'
         ORDER BY distance
         LIMIT 2",
    )?;

    if let Some(ExecutionResult::Query(result)) = results.last() {
        println!("{}", result.columns.join(" | "));
        for row in &result.rows {
            let values = row.iter().map(ToString::to_string).collect::<Vec<_>>();
            println!("{}", values.join(" | "));
        }
    }
    Ok(())
}
