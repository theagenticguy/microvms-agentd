"""Fixtures only. The fake daemon and its helpers live in `fake_daemon.py`.

Split so the tests can import the helpers by module name: a conftest is not an
importable package member, so `from .conftest import ...` fails under rootdir
collection from the repo root — which is exactly how the README says to run this.
"""

from __future__ import annotations

from collections.abc import Iterator

import pytest

from fake_daemon import FakeDaemon


@pytest.fixture
def daemon() -> Iterator[FakeDaemon]:
    server = FakeDaemon()
    server.start()
    try:
        yield server
    finally:
        server.stop()
