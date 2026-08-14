-- vectors quickstart
-- Run from the repository root with:
--   vectors
--   .read examples/quickstart.sql

CREATE TABLE IF NOT EXISTS tutorial_documents (
    id INTEGER PRIMARY KEY,
    title TEXT NOT NULL,
    category TEXT,
    published BOOLEAN,
    embedding VECTOR(3)
);

INSERT INTO tutorial_documents VALUES
    (1, 'Rust ownership', 'tech', TRUE,  ARRAY[1.0, 0.0, 0.0]),
    (2, 'Vector search',  'tech', TRUE,  ARRAY[0.9, 0.1, 0.0]),
    (3, 'SQL planner',    'tech', FALSE, ARRAY[0.7, 0.3, 0.0]),
    (4, 'Bread recipe',   'food', TRUE,  ARRAY[0.0, 1.0, 0.0])
ON CONFLICT (id) DO UPDATE SET
    title = excluded.title,
    category = excluded.category,
    published = excluded.published,
    embedding = excluded.embedding;

-- A scalar hash index lets the planner prune metadata before vector scoring.
CREATE INDEX IF NOT EXISTS tutorial_category_idx
ON tutorial_documents USING HASH (category);

-- Hybrid exact search: relational predicates and vector ranking in one query.
SELECT id,
       title,
       cosine_distance(embedding, ARRAY[1.0, 0.0, 0.0]) AS distance
FROM tutorial_documents
WHERE category = 'tech' AND published = TRUE
ORDER BY distance
LIMIT 3;

-- EXPLAIN reports index pruning, VectorTopK selection, and compute policy.
EXPLAIN SELECT id,
               title,
               cosine_distance(embedding, ARRAY[1.0, 0.0, 0.0]) AS distance
FROM tutorial_documents
WHERE category = 'tech'
ORDER BY distance
LIMIT 3;
