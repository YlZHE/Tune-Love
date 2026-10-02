"""Sender behavior without a host, plugin, OS lookup, RPC, or commercial data."""
from concurrent.futures import ThreadPoolExecutor
import importlib.util
import unittest


class SenderTests(unittest.TestCase):
    def setUp(self):
        self.assertIsNotNone(importlib.util.find_spec("reference.sender"),
                             "Implement the transport-independent sender broker")
        from reference.sender import MessageBroker
        self.broker = MessageBroker()

    def test_late_client_gets_latest_whole_message_per_command(self):
        self.broker.publish(b"PARAMS", b'{"retune":0.5,"flex":0.2}')
        self.broker.publish(b"PARAMS", b'{"flex":0.7}')
        self.assertEqual(self.broker.poll(11), b'{"flex":0.7}')
        self.assertIsNone(self.broker.poll(11))

    def test_seed_uses_byte_order_including_prefixes_and_non_ascii(self):
        for command, payload in [(b"a", b"lower"), (b"\xff", b"high"),
                                 (b"AB", b"long"), (b"A", b"short"),
                                 (b"\x80", b"middle")]:
            self.broker.publish(command, payload)
        self.assertEqual([self.broker.poll(11) for _ in range(6)],
                         [b"short", b"long", b"lower", b"middle", b"high", None])

    def test_established_client_preserves_publish_order_and_duplicates(self):
        self.assertIsNone(self.broker.poll(11))
        self.broker.publish(b"Z", b"one")
        self.broker.publish(b"A", b"two")
        self.broker.publish(b"Z", b"three")
        self.broker.publish(b"Z", b"three")
        self.assertEqual([self.broker.poll(11) for _ in range(5)],
                         [b"one", b"two", b"three", b"three", None])

    def test_each_client_consumes_its_own_queue(self):
        self.broker.poll(11)
        self.broker.poll(22)
        self.broker.publish(b"P", b"one")
        self.assertEqual(self.broker.poll(11), b"one")
        self.broker.publish(b"P", b"two")
        self.assertEqual(self.broker.poll(22), b"one")
        self.assertEqual(self.broker.poll(22), b"two")
        self.assertEqual(self.broker.poll(11), b"two")

    def test_empty_registered_client_is_not_seeded_again(self):
        self.broker.publish(b"P", b"one")
        self.assertEqual(self.broker.poll(11), b"one")
        self.assertIsNone(self.broker.poll(11))
        self.assertIsNone(self.broker.poll(11))
        self.assertEqual(self.broker.poll(22), b"one")

    def test_updates_during_seed_drain_follow_remaining_snapshot(self):
        self.broker.publish(b"A", b"old-a")
        self.broker.publish(b"B", b"old-b")
        self.assertEqual(self.broker.poll(11), b"old-a")
        self.broker.publish(b"B", b"new-b")
        self.assertEqual([self.broker.poll(11) for _ in range(3)],
                         [b"old-b", b"new-b", None])
        self.assertEqual([self.broker.poll(22) for _ in range(3)],
                         [b"old-a", b"new-b", None])

    def test_exactly_one_hundred_pending_messages_are_preserved(self):
        self.broker.poll(11)
        messages = [str(n).encode() for n in range(100)]
        for payload in messages:
            self.broker.publish(b"P", payload)
        self.assertEqual([self.broker.poll(11) for _ in range(101)], messages + [None])

    def test_overflow_discards_client_queue_then_reseeds_latest(self):
        self.broker.poll(11)
        for n in range(101):
            self.broker.publish(b"Z", str(n).encode())
        self.broker.publish(b"A", b"after-removal")
        self.assertEqual([self.broker.poll(11) for _ in range(3)],
                         [b"after-removal", b"100", None])

    def test_overflow_of_slow_client_does_not_reset_fast_client(self):
        self.broker.poll(11)
        self.broker.poll(22)
        for n in range(101):
            payload = str(n).encode()
            self.broker.publish(b"P", payload)
            self.assertEqual(self.broker.poll(22), payload)
        self.assertIsNone(self.broker.poll(22))
        self.assertEqual(self.broker.poll(11), b"100")
        self.assertIsNone(self.broker.poll(11))

    def test_seed_is_not_truncated_at_publish_overflow_limit(self):
        # The observed limit is checked on publish, not on initial snapshot creation.
        messages = [str(n).zfill(3).encode() for n in range(102)]
        for payload in messages:
            self.broker.publish(payload, payload)
        self.assertEqual([self.broker.poll(11) for _ in range(103)], messages + [None])

    def test_broker_does_not_interpret_clear_or_parameter_payloads(self):
        self.broker.publish(b"PARAMS", b"state")
        self.broker.publish(b"CLEAR", b"clear-command")
        self.assertEqual([self.broker.poll(11) for _ in range(3)],
                         [b"clear-command", b"state", None])

    def test_empty_bytes_are_distinct_from_no_message(self):
        self.broker.publish(b"", b"")
        self.assertEqual(self.broker.poll(0), b"")
        self.assertIsNone(self.broker.poll(0))

    def test_no_acknowledgement_or_retry_is_required_to_consume(self):
        self.broker.poll(11)
        self.broker.publish(b"P", b"first")
        self.broker.publish(b"P", b"second")
        self.assertEqual(self.broker.poll(11), b"first")
        # Downstream processing can fail; this layer already consumed the first message.
        self.assertEqual(self.broker.poll(11), b"second")
        self.assertIsNone(self.broker.poll(11))

    def test_invalid_inputs_do_not_replace_pending_or_latest_messages(self):
        self.broker.publish(b"P", b"good")
        for command, payload in [("P", b"bad"), (b"P", "bad"),
                                 (bytearray(b"P"), b"bad"), (b"P", bytearray(b"bad"))]:
            with self.subTest(command=command, payload=payload), self.assertRaises(TypeError):
                self.broker.publish(command, payload)
        for client_id in [-1, 2**32, True, "11", 11.0, None]:
            with self.subTest(client_id=client_id), self.assertRaises((TypeError, ValueError)):
                self.broker.poll(client_id)
        self.assertEqual(self.broker.poll(11), b"good")
        self.assertIsNone(self.broker.poll(11))

    def test_concurrent_polls_consume_each_pending_message_once(self):
        self.broker.poll(11)
        messages = [str(n).encode() for n in range(80)]
        for payload in messages:
            self.broker.publish(b"P", payload)
        with ThreadPoolExecutor(max_workers=8) as pool:
            results = list(pool.map(self.broker.poll, [11] * 90))
        self.assertCountEqual([r for r in results if r is not None], messages)
        self.assertEqual(results.count(None), 10)


if __name__ == "__main__":
    unittest.main()
