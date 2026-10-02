"""Transport-neutral model of the observed Kaka sender broker.

This module intentionally contains no Windows, RPC, host, plugin, or audio code.
It models only the queue contract supported by the local static evidence:

* one latest serialized payload is retained for each command;
* a client creates a private FIFO on its first poll, seeded in command-byte order;
* later publishes append to already registered client queues, including duplicates;
* a queue with more than 100 pending messages is removed as a whole;
* polling consumes one message and does not require an acknowledgement.

The broker does not interpret command names or payloads. In particular, it does
not decide when a parameter cache should be cleared or how six plugin parameters
are assembled.
"""
from __future__ import annotations

from collections import deque
from threading import RLock


class MessageBroker:
    """Thread-safe sender core; no host or transport is connected automatically.

    The 100-message limit applies to registered queues during publish. The
    latest-command map and number of clients have no inferred global cap. A
    transport adapter must authenticate callers and check process identity;
    a numeric ID alone is not authorization to control a process.
    """

    MAX_PENDING = 100
    MAX_CLIENT_ID = 2**32 - 1

    def __init__(self):
        self._lock = RLock()
        self._latest: dict[bytes, bytes] = {}
        self._queues: dict[int, deque[bytes]] = {}

    @staticmethod
    def _command(value: bytes) -> bytes:
        if type(value) is not bytes:
            raise TypeError("command must be bytes")
        return value

    @staticmethod
    def _payload(value: bytes) -> bytes:
        if type(value) is not bytes:
            raise TypeError("payload must be bytes")
        return value

    @classmethod
    def _client_id(cls, value: int) -> int:
        if type(value) is not int:
            raise TypeError("client_id must be an integer")
        if not 0 <= value <= cls.MAX_CLIENT_ID:
            raise ValueError("client_id must be an unsigned 32-bit integer")
        return value

    def publish(self, command: bytes, payload: bytes) -> None:
        """Publish a complete serialized payload for ``command``.

        The latest map is updated even when no clients are registered. Existing
        clients receive the payload independently, so a repeated command is not
        coalesced in their pending FIFO.
        """
        command = self._command(command)
        payload = self._payload(payload)
        with self._lock:
            self._latest[command] = payload
            overflowed = []
            for client_id, queue in self._queues.items():
                queue.append(payload)
                if len(queue) > self.MAX_PENDING:
                    overflowed.append(client_id)
            for client_id in overflowed:
                # The observed sender drops the client's registration, rather
                # than trimming only its oldest item.
                del self._queues[client_id]

    def poll(self, client_id: int) -> bytes | None:
        """Return and consume one payload for a client, or ``None`` if empty."""
        client_id = self._client_id(client_id)
        with self._lock:
            queue = self._queues.get(client_id)
            if queue is None:
                # ``sorted`` gives the same bytewise ordering as the observed
                # string-tree traversal. This snapshot is made atomically with
                # registration, so later publishes append after the seed.
                queue = deque(self._latest[key] for key in sorted(self._latest))
                self._queues[client_id] = queue
            if not queue:
                return None
            return queue.popleft()

