"""Errors raised by textdb, mapped from the SQLSTATE-style codes TX001–TX005."""

import json
from typing import Any, Dict, Optional


class TextdbError(Exception):
    """Base class. `code` is TX000–TX005; `payload` is the parsed JSON detail when present."""

    code = "TX000"

    def __init__(self, message: str, code: Optional[str] = None, payload: Optional[Dict[str, Any]] = None):
        super().__init__(message)
        self.message = message
        if code:
            self.code = code
        self.payload = payload or {}


class Conflict(TextdbError):
    """Another writer changed the same lines. `theirs` is the *current* text of the region,
    `base` the text both edits started from, `ours` the caller's version, and
    `current_version` the version to pass as `base_version` on retry."""

    code = "TX001"

    @property
    def theirs(self) -> str:
        return self.payload.get("theirs", "")

    @property
    def base(self) -> str:
        return self.payload.get("base", "")

    @property
    def ours(self) -> str:
        return self.payload.get("ours", "")

    @property
    def current_version(self) -> Optional[int]:
        return self.payload.get("current_version")

    @property
    def region(self):
        return self.payload.get("region_line_from"), self.payload.get("region_line_to")


class Contention(TextdbError):
    code = "TX002"


class NotFound(TextdbError):
    code = "TX003"


class InvalidEdit(TextdbError):
    code = "TX004"


class Forbidden(TextdbError):
    """In your view and not yours to do: a read-only share written to, an owner-only operation.

    Distinct from NotFound, which is what a path outside every share answers, and the difference
    is the point of having both.
    """

    code = "TX005"


_BY_CODE = {
    "TX001": Conflict,
    "TX002": Contention,
    "TX003": NotFound,
    "TX004": InvalidEdit,
    "TX005": Forbidden,
}


def from_code(code: str, message: str, detail: Optional[str] = None) -> TextdbError:
    payload: Dict[str, Any] = {}
    if detail:
        try:
            payload = json.loads(detail)
        except ValueError:
            payload = {"detail": detail}
    cls = _BY_CODE.get(code, TextdbError)
    return cls(message, code, payload)


def from_message(message: str) -> Optional[TextdbError]:
    """Parse the SQLite form: 'TX001 conflict: {json}' / 'TX004 invalid edit: …'."""
    for code in ("TX001", "TX002", "TX003", "TX004", "TX005", "TX000"):
        idx = message.find(code)
        if idx >= 0:
            rest = message[idx + len(code):].strip()
            detail = None
            brace = rest.find("{")
            if code == "TX001" and brace >= 0:
                detail = rest[brace:]
                rest = rest[:brace].rstrip(": ")
            return from_code(code, rest or message, detail)
    return None
