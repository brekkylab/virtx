"""A sandbox's ports as ``set_inbound`` leaves them, against a client that answers as virtx
cloud does: no server, no machine."""

import asyncio
from typing import Any

from virtx.sandbox import PortInbound, Sandbox, SandboxPort, SandboxState


class Answering:
    """Answers ``set_sandbox_port_inbound`` with ``address``, as virtx cloud does."""

    def __init__(self, address: str | None) -> None:
        self.address = address

    async def set_sandbox_port_inbound(self, id: str, port: int, rules: Any) -> PortInbound:
        return PortInbound(port, [], self.address)


def sandbox(address: str | None) -> Sandbox:
    return Sandbox(
        client=Answering(address),  # type: ignore[arg-type]
        id="sbx_one",
        name=None,
        image="sha256:00",
        state=SandboxState.RUNNING,
        ephemeral=False,
        auto_stop_minutes=15,
        auto_delete_days=30,
        used_at="2026-11-13T00:00:00Z",
        ports=[SandboxPort(8000, "wss://p/8000"), SandboxPort(9000, "wss://p/9000")],
    )


def test_the_address_set_inbound_answers_is_kept_on_its_port() -> None:
    """Issue #184: ``ports`` gives a TCP port the public address its rules gave it, and takes it
    away once a rule set answers none; the other ports are left as they were."""
    given = sandbox("203.0.113.7:30001")
    asyncio.run(given.set_inbound(8000, [{"source": "203.0.113.0/24"}]))
    assert [p.address for p in given.ports] == ["203.0.113.7:30001", None]

    taken = sandbox(None)
    taken.ports[0] = SandboxPort(8000, "wss://p/8000", address="203.0.113.7:30001")
    asyncio.run(taken.set_inbound(8000, []))
    assert [p.address for p in taken.ports] == [None, None]
