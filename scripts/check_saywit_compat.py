#!/usr/bin/env python3
"""Exercise Saywit's real transport against an isolated, durable Vectors server.

Run with Saywit's Python environment (Django is required). Only synthetic rows
and embeddings are used. No application settings, messages, keys, or providers
are loaded. The supplied checkout is read, never modified.
"""
import argparse
import importlib.util
import json
from pathlib import Path
import tempfile
import tomllib
from unittest.mock import patch

from release_smoke import require, running_server


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--saywit", type=Path, required=True)
    parser.add_argument("--server", type=Path, required=True)
    args = parser.parse_args()
    from django.conf import settings

    settings.configure(
        SAYWIT_VECTORS_URL="", SAYWIT_VECTORS_TOKEN="", VOYAGE_API_KEY="",
        CACHES={"default": {"BACKEND": "django.core.cache.backends.locmem.LocMemCache"}},
    )
    path = args.saywit.resolve() / "semantic" / "transport.py"
    spec = importlib.util.spec_from_file_location("saywit_transport_contract", path)
    require(spec is not None and spec.loader is not None, "Saywit transport not found")
    transport = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(transport)
    version = tomllib.loads((Path(__file__).resolve().parents[1] / "Cargo.toml").read_text())["package"]["version"]
    vector = [1.0] + [0.0] * (transport.DIMENSIONS - 1)
    second = [0.8, 0.2] + [0.0] * (transport.DIMENSIONS - 2)
    fixtures = [
        ("private", "private-chat", "private-turn", transport.PROFILE, vector),
        ("old", "team-a", "old-turn", "incompatible-profile", vector),
        ("self", "team-a", "self-turn", transport.PROFILE, vector),
        ("answer-a", "team-a", "answer-turn-a", transport.PROFILE, second),
        ("answer-b", "team-b", "answer-turn-b", transport.PROFILE, second),
        ("quoted", "team-a' OR 1=1 --", "quoted-turn", transport.PROFILE, vector),
    ]
    rows = [
        {"id": identity, "chat_id": chat, "turn_id": turn, "source_id": index + 1,
         "profile": profile, "embedding": embedding}
        for index, (identity, chat, turn, profile, embedding) in enumerate(fixtures)
    ]
    edited = dict(rows[4], chat_id="team-c", turn_id="edited-turn", embedding=vector)

    def connect(api):
        settings.SAYWIT_VECTORS_URL = f"http://127.0.0.1:{api.port}"
        settings.SAYWIT_VECTORS_TOKEN = api.token
        transport.ensure_schema()

    def search():
        seen = []
        original_sql = transport.sql

        def record(statement, parameters=()):
            result = original_sql(statement, parameters)
            seen.append(result)
            return result

        with patch.object(transport, "sql", side_effect=record):
            hits = transport.nearest(vector, ["team-a", "team-b", "team-a"], limit=2, exclude_turn="self-turn")
        require([h["id"] for h in hits] == ["answer-a", "answer-b"], "scope/profile/exclusion or tie order mismatch")
        require(seen[0]["rows_examined"] == 4, "chat membership did not use indexed candidates")
        require(all(0.9 < h["score"] < 1.0 for h in hits), "cosine scores changed")
        quoted = transport.nearest(vector, ["team-a' OR 1=1 --"], limit=1)
        require([h["id"] for h in quoted] == ["quoted"], "bound text was not kept literal")
        many = transport.nearest(vector, ["team-b", *[f"missing-{n}" for n in range(499)]], limit=100)
        require([h["id"] for h in many] == ["answer-b"], "500-chat scope mismatch")
        require(transport.nearest(vector, []) == [], "empty scope returned results")

    with tempfile.TemporaryDirectory(prefix="vectors-saywit-") as temporary:
        directory = Path(temporary)
        with running_server(args.server.resolve(), directory, 30, version) as api:
            connect(api)
            transport.insert_units(rows)
            transport.insert_units(rows)
            require(transport.sql(f"SELECT COUNT(*) FROM {transport.TABLE}")["rows"] == [[len(rows)]], "retry duplicated units")
            search()
            # A failed batch must not expose its earlier edit to assistant retrieval.
            try:
                transport.insert_units([edited, rows[0], rows[0]])
            except transport.SemanticError:
                pass
            else:
                raise AssertionError("duplicate update was accepted")
            search()
            transport.insert_units([edited])
            require(transport.nearest(vector, ["team-b"]) == [], "old chat scope retained edited unit")
        with running_server(args.server.resolve(), directory, 30, version) as api:
            connect(api)
            require(transport.nearest(vector, ["team-b"]) == [], "old scope returned after recovery")
            hits = transport.nearest(vector, ["team-c"])
            require(len(hits) == 1 and hits[0]["id"] == "answer-b" and hits[0]["score"] == 1.0, "edited vector or scope not recovered")
            transport.insert_units(rows)
            search()
            transport.sql(f"DELETE FROM {transport.TABLE} WHERE turn_id=$1", ["answer-turn-b"])
            require(transport.nearest(vector, ["team-b"]) == [], "deleted turn remained searchable")
    print(json.dumps({"status": "passed", "version": version,
                      "transport": str(path), "dimensions": transport.DIMENSIONS,
                      "checks": ["real HTTP transport", "schema", "normalized bulk insert", "scope before ranking",
                                 "profile and turn exclusion", "duplicate IDs", "bound quoted IDs", "500-chat scope",
                                 "exact cosine scores", "idempotent retry", "atomic rejected batch",
                                 "edit and chat move", "durable restart", "deletion"],
                      "provider_calls": 0, "user_messages_read": 0}))


if __name__ == "__main__":
    main()
