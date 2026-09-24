"""Fixture and output locations for the ported upstream Python tests.

Upstream reads fixtures next to its tests and writes outputs there too; the
port reads the vendored copies under ``PhotoshopAPI-rs/fixtures`` and writes
into a per-run temporary directory so fixtures are never modified.
"""

import os
import tempfile

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", "..", "..", ".."))
FIXTURES = os.path.join(ROOT, "fixtures")
OUTPUT = tempfile.mkdtemp(prefix="psapi-upstream-")


def python_fixture(*parts):
    """A file from fixtures/python (upstream's psapi-test data)."""
    return os.path.join(FIXTURES, "python", *parts)


def document_fixture(*parts):
    """A file from fixtures/documents (upstream's PhotoshopTest/documents)."""
    return os.path.join(FIXTURES, "documents", *parts)


def output(*parts):
    """A path in the temporary output directory."""
    return os.path.join(OUTPUT, *parts)
