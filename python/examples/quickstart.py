"""Run against a new/local Vectors server: python python/examples/quickstart.py."""

import os
import uuid

from vectors_sdk import Client, QueryResult

with Client(
    os.getenv("VECTORS_URL", "http://127.0.0.1:8080"),
    token=os.getenv("VECTORS_API_TOKEN"),
) as db:
    # Generated identifier contains only ASCII letters and hex digits.
    table = "sdk_demo_" + uuid.uuid4().hex[:12]
    db.execute(
        f"CREATE TABLE {table} (id INTEGER PRIMARY KEY, text TEXT, embedding VECTOR(3))"
    )
    try:
        summary = db.insert(
            table,
            (
                {"id": i, "text": f"Passage {i}", "embedding": [1, i, 0]}
                for i in range(10)
            ),
            batch_size=3,
        )
        print(summary)
        result = db.execute(
            f"SELECT id, text, embedding <=> $1 AS distance FROM {table} ORDER BY distance LIMIT $2",
            [[1, 0, 0], 3],
        )[0]
        assert isinstance(result, QueryResult)
        print(result.to_dicts())
        print(
            db.preview_chunks(
                "# Recovery\nCommitted records are replayed from the WAL."
            )
        )
    finally:
        db.execute(f"DROP TABLE {table}")
