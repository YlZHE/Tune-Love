"""Real Client/profile/wire exercised against deterministic OS and pipe boundaries."""
import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
try:
    import app_bridge
except ImportError:
    app_bridge = None
from reference.tests.test_client import pe_bytes, fixture_profile, make_junction


class BridgeTests(unittest.TestCase):
    def setUp(self):
        self.assertIsNotNone(app_bridge, "app_bridge must implement the application boundary")
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.plugin = Path(self.temp.name) / "Auto-Tune fixture.vst3"
        self.plugin.write_bytes(pe_bytes())
        self.profile = fixture_profile(self.plugin)
        self.identity = {"pid": 1234, "created_filetime": 2**60, "executable": r"C:\fixture.exe",
                         "architecture": "x64", "same_user": True}
        self.loaded = [{"name": self.plugin.name, "path": str(self.plugin)}]
        self.matched = True
        self.revision = 0
        self.consumed = 0
        self.packets = []
        self.extra_instances = []
        self.overflow = 0
        self.snapshot_available = True
        self.bridge = self.make_bridge()

    def make_bridge(self):
        return app_bridge.AppBridge(
            profiles=[self.profile], scan_fn=lambda: {"ok": True, "targets": [
                {"identity": dict(self.identity), "plugins": list(self.loaded)}], "errors": [{}, {}]},
            identity_reader=lambda pid: dict(self.identity), module_reader=lambda pid: list(self.loaded),
            transport=self.transport)

    def transport(self, pid, packet):
        import struct
        self.packets.append(packet)
        _, _, op, group, size, serial = struct.unpack_from("<IHHIIQ", packet)
        if op == 5:
            return b'{"ok":true,"group":1}'
        if op == 3:
            self.revision += 1
            return json.dumps({"ok": True, "stage": "cached", "duplicate": False,
                               "cache_revision": self.revision}).encode()
        if op == 4:
            self.revision = 0
            return b'{"ok":true,"cleared":true,"dsp_reset":false}'
        if op == 1:
            return json.dumps({"ok": True, "route": "direct_process_queue", "instance_overflow": self.overflow,
                "groups": [{"group": 1, "snapshot_available": self.snapshot_available,
                            "cache_revision": self.revision if self.snapshot_available else 0}],
                "instances": ([{"index": 0, "group": 1, "state": "matched",
                    "consumed_revision": self.consumed, "submitted": self.consumed,
                    "completed_revision": self.consumed, "completion_result": 0,
                    "last_process_result": 0}] if self.matched else []) + self.extra_instances}).encode()
        self.fail("Unexpected wire operation (including implicit clear)")

    def connect(self):
        scanned = self.bridge.command({"op": "scan"})
        self.assertEqual(scanned["skippedCount"], 2)
        connected = self.bridge.command({"op": "connect", "candidateId": scanned["candidates"][0]["candidateId"]})
        self.assertTrue(connected["ok"], connected)
        return connected["state"]["connectionId"]

    def apply(self, connection, sequence=1, values=None):
        return self.bridge.command({"op": "apply", "connectionId": connection, "sequence": sequence,
                                    "values": {"retune": .5} if values is None else values})

    def test_connect_works_when_the_host_loaded_the_plugin_through_a_junction(self):
        real = Path(self.temp.name) / "real-vst3"
        real.mkdir()
        (real / self.plugin.name).write_bytes(pe_bytes())
        link = Path(self.temp.name) / "linked-vst3"
        make_junction(link, real)
        self.plugin = link / self.plugin.name
        self.profile = fixture_profile(self.plugin)
        self.loaded = [{"name": self.plugin.name, "path": str(self.plugin)}]
        self.bridge = self.make_bridge()
        self.connect()

    def test_disconnected_and_connect_have_no_implicit_values_or_clear(self):
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["phase"], "disconnected")
        self.assertFalse(self.apply("none")["ok"])
        self.connect()
        import struct
        self.assertEqual([struct.unpack_from("<H", p, 6)[0] for p in self.packets], [5, 1])
        self.bridge.command({"op": "disconnect"})
        self.assertEqual(len(self.packets), 2)

    def test_options_are_read_only_and_return_profile_values(self):
        scanned = self.bridge.command({"op": "scan"})
        candidate = scanned["candidates"][0]
        response = self.bridge.command({"op": "options", "candidateId": candidate["candidateId"], "role": "key"})
        self.assertTrue(response["ok"], response)
        self.assertEqual(response["options"]["source"], "profile")
        self.assertEqual(response["options"]["profile_id"], "fixture-0")
        self.assertEqual(response["options"]["role"], "key")
        self.assertEqual(response["options"]["options"], [
            {"label": "C", "normalized": 0.0},
            {"label": "F#", "normalized": 6 / 11},
            {"label": "B", "normalized": 1.0},
        ])
        self.assertEqual(self.packets, [])

    def test_explicit_clear_is_the_only_application_cache_clear_path(self):
        connection = self.connect()
        self.assertTrue(self.apply(connection)["ok"])
        self.consumed = 1
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["delivery"]["stage"], "submitted")
        response = self.bridge.command({"op": "clear", "connectionId": connection})
        self.assertTrue(response["ok"], response)
        self.assertEqual(response["state"]["phase"], "ready")
        self.assertIsNone(response["state"]["delivery"])
        import struct
        operations = [struct.unpack_from("<H", p, 6)[0] for p in self.packets]
        self.assertEqual(operations[-1], 4)
        self.assertEqual(operations.count(4), 1)

    def test_stale_clear_preserves_replacement_connection_and_sends_nothing(self):
        old = self.connect()
        new = self.connect()
        self.assertTrue(self.apply(new)["ok"])
        count = len(self.packets)
        response = self.bridge.command({"op": "clear", "connectionId": old})
        self.assertFalse(response["ok"])
        self.assertEqual(response["state"]["connectionId"], new)
        self.assertEqual(response["state"]["delivery"], {"stage": "cached", "sequence": 1})
        self.assertEqual(len(self.packets), count)
        self.assertTrue(self.apply(new, 2)["ok"])

    def test_clear_rejects_missing_or_false_dsp_contract_and_does_not_retry(self):
        import struct
        for result in ({"ok": True, "cleared": True},
                       {"ok": True, "cleared": True, "dsp_reset": True},
                       {"ok": True, "cleared": True, "dsp_reset": 0}):
            connection = self.connect()
            real_transport = self.transport
            def transport(pid, packet):
                if struct.unpack_from("<H", packet, 6)[0] == 4:
                    self.packets.append(packet)
                    return json.dumps(result).encode()
                return real_transport(pid, packet)
            self.bridge.controller._transport = transport
            count = len(self.packets)
            response = self.bridge.command({"op": "clear", "connectionId": connection})
            self.assertFalse(response["ok"], result)
            self.assertIsNone(response["state"]["connectionId"])
            self.assertEqual(sum(struct.unpack_from("<H", p, 6)[0] == 4
                                 for p in self.packets[count:]), 1)

    def test_awaiting_rejects_apply(self):
        self.matched = False
        connection = self.connect()
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["phase"], "awaiting")
        self.assertFalse(self.apply(connection)["ok"])

    def test_cached_is_not_submitted_until_all_instances_consume(self):
        connection = self.connect()
        result = self.apply(connection)
        self.assertTrue(result["ok"])
        self.assertEqual(result["state"]["delivery"], {"stage": "cached", "sequence": 1})
        self.consumed = 1
        state = self.bridge.command({"op": "status"})["state"]
        self.assertEqual(state["delivery"]["stage"], "submitted")
        self.assertFalse(state["audioVerified"])

    def test_wire_uses_profile_id_and_separate_nonzero_serial(self):
        import struct
        connection = self.connect()
        self.assertTrue(self.apply(connection, 17, {"flex": .25})["ok"])
        packet = next(p for p in self.packets if struct.unpack_from("<H", p, 6)[0] == 3)
        self.assertEqual(struct.unpack_from("<If", packet, 24), (90, .25))
        self.assertNotEqual(struct.unpack_from("<Q", packet, 16)[0], 17)

    def test_consumed_revision_with_old_success_is_not_completion(self):
        connection = self.connect()
        self.assertTrue(self.apply(connection)["ok"])
        self.consumed = 1
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["delivery"]["stage"], "submitted")
        self.assertTrue(self.apply(connection, 2)["ok"])
        self.consumed = 2
        self.extra_instances = [{"index": 1, "group": 1, "state": "matched",
            "consumed_revision": 2, "submitted": 2, "last_process_result": 0,
            "completed_revision": 1, "completion_result": 0}]
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["delivery"]["stage"], "cached")
        self.extra_instances[0].update(completed_revision=2, completion_result=1)
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["delivery"]["stage"], "cached")
        self.extra_instances[0]["completion_result"] = 0
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["delivery"]["stage"], "submitted")

    def test_legacy_agent_without_completion_does_not_confirm_delivery(self):
        connection = self.connect()
        self.apply(connection)
        self.consumed = 1
        self.extra_instances = [{"index": 1, "group": 1, "state": "matched",
            "consumed_revision": 1, "submitted": 1, "last_process_result": 0}]
        state = self.bridge.command({"op": "status"})["state"]
        self.assertEqual(state["delivery"]["stage"], "cached")

    def test_busy_cache_snapshot_keeps_connection_and_waits_for_fresh_status(self):
        connection = self.connect()
        self.apply(connection)
        self.consumed = 1
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["delivery"]["stage"], "submitted")
        self.snapshot_available = False
        response = self.bridge.command({"op": "status"})
        self.assertTrue(response["ok"], response)
        self.assertEqual(response["state"]["connectionId"], connection)
        self.assertEqual(response["state"]["phase"], "awaiting")
        self.assertEqual(response["state"]["delivery"]["stage"], "cached")
        self.snapshot_available = True
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["delivery"]["stage"], "submitted")

    def test_busy_snapshot_rejects_writes_without_losing_session_or_retry(self):
        import struct
        for op in ("apply", "clear"):
            self.snapshot_available = True
            connection = self.connect()
            self.assertTrue(self.apply(connection)["ok"])
            self.snapshot_available = False
            count = len(self.packets)
            response = (self.apply(connection, 2) if op == "apply" else
                        self.bridge.command({"op": "clear", "connectionId": connection}))
            self.assertFalse(response["ok"])
            self.assertEqual(response["state"]["connectionId"], connection)
            self.assertEqual(response["state"]["phase"], "awaiting")
            self.assertEqual([struct.unpack_from("<H", p, 6)[0] for p in self.packets[count:]], [1])
            self.snapshot_available = True
            self.assertEqual(self.bridge.command({"op": "status"})["state"]["phase"], "ready")
            self.assertTrue(self.apply(connection, 2)["ok"])

    def test_key_and_scale_labels_apply_through_the_profile_options(self):
        connection = self.connect()
        self.assertIn("key", self.bridge.state["capabilities"])
        self.assertIn("scale", self.bridge.state["capabilities"])
        response = self.apply(connection, values={"retune": .5, "key": "F#", "scale": "Minor"})
        self.assertTrue(response["ok"], response)
        self.assertEqual(response["state"]["delivery"], {"stage": "cached", "sequence": 1})

    def test_invalid_values_fail_closed(self):
        for values in ({}, {"retune": float("nan")}, {"retune": 1.1}, {"retune": True},
                       {"key": .4}, {"key": "H"}, {"key": ""}, {"scale": 2 / 14}, {"bogus": .4}):
            connection = self.connect()
            response = self.apply(connection, values=values)
            self.assertFalse(response["ok"], values)
            self.assertIsNone(response["state"]["connectionId"])

    def test_replay_and_old_connection_fail_closed(self):
        old = self.connect()
        self.assertTrue(self.apply(old)["ok"])
        self.assertFalse(self.apply(old)["ok"])
        new = self.connect()
        self.assertNotEqual(old, new)
        self.assertFalse(self.apply(old, 2)["ok"])

    def test_identity_reuse_user_change_unload_and_profile_mismatch(self):
        for mutation in ("created_filetime", "same_user", "unload", "sha"):
            connection = self.connect()
            original_identity, original_loaded = dict(self.identity), list(self.loaded)
            if mutation == "created_filetime":
                self.identity["created_filetime"] += 1
            elif mutation == "same_user":
                self.identity["same_user"] = False
            elif mutation == "unload":
                self.loaded = []
            else:
                self.plugin.write_bytes(pe_bytes() + b"changed")
            result = self.apply(connection)
            self.assertFalse(result["ok"], mutation)
            self.assertIsNone(result["state"]["connectionId"])
            self.identity, self.loaded = original_identity, original_loaded
            self.plugin.write_bytes(pe_bytes())

    def test_rescan_expires_candidate_not_connection_and_unsupported_is_visible(self):
        first = self.bridge.command({"op": "scan"})["candidates"][0]["candidateId"]
        connection = self.connect()
        self.bridge.command({"op": "scan"})
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["connectionId"], connection)
        self.assertFalse(self.bridge.command({"op": "connect", "candidateId": first})["ok"])
        self.plugin.write_bytes(pe_bytes() + b"unknown")
        candidate = self.bridge.command({"op": "scan"})["candidates"][0]
        self.assertFalse(candidate["compatible"])
        self.assertIsNone(candidate["profileId"])

    def test_capabilities_are_exact_profile_subset(self):
        del self.profile["roles"]["flex"]
        self.bridge = self.make_bridge()
        connection = self.connect()
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["capabilities"],
                         ["retune", "vibrato", "humanize", "key", "scale"])
        self.assertFalse(self.apply(connection, values={"flex": .2})["ok"])

    def test_unknown_fields_and_js_unsafe_sequence_rejected(self):
        self.assertFalse(self.bridge.command([])["ok"])
        self.assertFalse(self.bridge.command({"op": "scan", "path": "anything"})["ok"])
        connection = self.connect()
        self.assertFalse(self.apply(connection, 2**53)["ok"])

    def test_scan_failure_expires_candidates_but_preserves_connection(self):
        connection = self.connect()
        def failing_scan():
            raise OSError("scan unavailable")
        self.bridge.scan_fn = failing_scan
        response = self.bridge.command({"op": "scan"})
        self.assertFalse(response["ok"])
        self.assertEqual(response["state"]["connectionId"], connection)
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["phase"], "ready")

    def test_all_matched_instances_must_consume_and_external_cache_change_fails(self):
        connection = self.connect()
        self.apply(connection)
        self.consumed = 1
        self.extra_instances = [{"index": 1, "group": 1, "state": "matched",
                                 "consumed_revision": 0, "submitted": 0, "last_process_result": 0,
                                 "completed_revision": 0, "completion_result": 0}]
        state = self.bridge.command({"op": "status"})["state"]
        self.assertEqual(state["instanceCount"], 2)
        self.assertEqual(state["delivery"]["stage"], "cached")
        self.extra_instances[0].update(consumed_revision=1, submitted=1, completed_revision=1)
        self.assertEqual(self.bridge.command({"op": "status"})["state"]["delivery"]["stage"], "submitted")
        self.revision = 2
        self.assertFalse(self.bridge.command({"op": "status"})["ok"])

    def test_conflicting_helper_rejected_before_attach(self):
        scanned = self.bridge.command({"op": "scan"})
        self.loaded.append({"name": "em64.dll", "path": r"C:\commercial\em64.dll"})
        result = self.bridge.command({"op": "connect", "candidateId": scanned["candidates"][0]["candidateId"]})
        self.assertFalse(result["ok"])
        self.assertEqual(self.packets, [])

    def test_second_same_name_reference_agent_rejected_before_apply(self):
        connection = self.connect()
        self.loaded.append({"name": "reference_agent.dll", "path": r"C:\other\reference_agent.dll"})
        result = self.apply(connection)
        self.assertFalse(result["ok"])

    def test_stale_connection_rejection_keeps_new_connection_ready(self):
        old = self.connect()
        new = self.connect()
        result = self.apply(old, 1)
        self.assertFalse(result["ok"])
        self.assertEqual(result["state"]["connectionId"], new)
        self.assertEqual(result["state"]["phase"], "ready")
        self.assertTrue(self.apply(new, 1)["ok"])

    def test_instance_overflow_never_ready_or_applied(self):
        connection = self.connect()
        self.overflow = 1
        packets = len(self.packets)
        result = self.apply(connection)
        self.assertFalse(result["ok"])
        self.assertIsNone(result["state"]["connectionId"])
        import struct
        self.assertNotIn(3, [struct.unpack_from("<H", p, 6)[0] for p in self.packets[packets:]])

if __name__ == "__main__":
    unittest.main()
