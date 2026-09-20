"""SQL and ingestion results. Graph responses preserve the server's JSON fields."""

from dataclasses import dataclass
from typing import Any

from .errors import ProtocolError


@dataclass(frozen=True)
class Column:
    name: str
    data_type: str | None


@dataclass(frozen=True)
class QueryResult:
    columns: list[str]
    schema: list[Column]
    rows: list[list[Any]]
    row_count: int
    rows_examined: int

    def to_dicts(self) -> list[dict[str, Any]]:
        """Return named rows; use SQL aliases when projection names collide."""
        if len(set(self.columns)) != len(self.columns):
            raise ValueError(
                "duplicate column names; add unique SQL aliases before using to_dicts()"
            )
        return [dict(zip(self.columns, row)) for row in self.rows]


@dataclass(frozen=True)
class CommandResult:
    tag: str
    rows_affected: int


@dataclass(frozen=True)
class IngestBatch:
    input_offset: int
    row_count: int
    rows_affected: int


@dataclass(frozen=True)
class IngestSummary:
    input_rows: int
    rows_affected: int
    batches: int


def parse_result(value: Any) -> QueryResult | CommandResult:
    try:
        if not isinstance(value, dict):
            raise ValueError("expected an object")
        if (
            value["type"] == "command"
            and isinstance(value["tag"], str)
            and _count(value["rows_affected"])
        ):
            return CommandResult(value["tag"], value["rows_affected"])
        if value["type"] == "query":
            columns, rows = value["columns"], value["rows"]
            if (
                not isinstance(columns, list)
                or not all(isinstance(column, str) for column in columns)
                or not isinstance(rows, list)
                or not _count(value["row_count"])
                or not _count(value["rows_examined"])
                or len(rows) != value["row_count"]
                or any(
                    not isinstance(row, list) or len(row) != len(columns)
                    for row in rows
                )
                or not isinstance(value["schema"], list)
                or len(value["schema"]) != len(columns)
            ):
                raise ValueError("inconsistent row dimensions")
            schema = [Column(**column) for column in value["schema"]]
            if any(
                column.name != name
                or (
                    column.data_type is not None
                    and not isinstance(column.data_type, str)
                )
                for column, name in zip(schema, columns)
            ):
                raise ValueError("inconsistent result schema")
            return QueryResult(
                columns,
                schema,
                rows,
                value["row_count"],
                value["rows_examined"],
            )
    except (KeyError, TypeError, ValueError):
        pass
    raise ProtocolError("invalid SQL result from server")


def _count(value: Any) -> bool:
    return isinstance(value, int) and not isinstance(value, bool) and value >= 0


def parse_results(value: Any) -> list[QueryResult | CommandResult]:
    if not isinstance(value, dict) or not isinstance(value.get("results"), list):
        raise ProtocolError("missing SQL results from server")
    return [parse_result(result) for result in value["results"]]
