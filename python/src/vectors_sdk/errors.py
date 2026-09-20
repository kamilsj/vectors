"""Structured errors, including progress for partially committed bulk loads."""


class VectorsError(Exception):
    """Base class for SDK and server failures."""


class APIError(VectorsError):
    def __init__(
        self,
        status_code: int,
        code: str,
        message: str,
        retry_after: float | None = None,
    ):
        self.status_code = status_code
        self.code = code
        self.message = message
        self.retry_after = retry_after
        super().__init__(f"{status_code} {code}: {message}")


class TransportError(VectorsError):
    """Connection/timeout failure. A submitted write may already have committed."""


class ProtocolError(VectorsError):
    """The server returned an unexpected response."""


class BulkInsertError(VectorsError):
    """Earlier batches committed; the failed batch's outcome may be unknown.

    input_offset is the number of input rows acknowledged before this failure,
    including conflict-skipped rows. Never resume blindly after a transport error.
    """

    def __init__(
        self,
        input_offset: int,
        rows_affected: int,
        batches_completed: int,
        cause: Exception,
    ):
        self.input_offset = input_offset
        self.rows_affected = rows_affected
        self.batches_completed = batches_completed
        self.cause = cause
        super().__init__(
            f"bulk insertion failed after {batches_completed} acknowledged batches ({input_offset} input rows)"
        )
