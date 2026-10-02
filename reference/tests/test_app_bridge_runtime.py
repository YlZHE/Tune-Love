"""Owned fixture -> real JSON-lines worker -> Client -> agent -> process queue.

Never selects an existing process. Temporary debug reference contains only
independent source/agent and a profile generated from the new fixture.
"""
import json
import os
from pathlib import Path
import queue
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest

from reference.tests.test_runtime import FixtureHost, make_profile, HOST, AGENT, ROOT


@unittest.skipUnless(os.name == "nt" and HOST.exists() and AGENT.exists(), "Built Windows fixture required")
class BridgeRuntimeTests(unittest.TestCase):
    def test_owned_fixture_four_values_via_persistent_worker(self):
        host = FixtureHost()
        self.addCleanup(lambda: host.close() if host.process.poll() is None else None)
        original = host.command("status")
        info = host.ready["modules"][0]
        profile = make_profile(info["path"], info["classes"][0], "app-bridge-owned-fixture")
        with tempfile.TemporaryDirectory(prefix="autotune-bridge-") as directory:
            reference = Path(directory)
            for name in ("app_bridge.py", "client.py"):
                shutil.copyfile(ROOT / name, reference / name)
            (reference / "profiles").mkdir()
            (reference / "profiles" / "fixture.json").write_text(json.dumps(profile), encoding="utf-8")
            (reference / "build" / "x64").mkdir(parents=True)
            shutil.copyfile(AGENT, reference / "build" / "x64" / AGENT.name)
            # Limit existing scan to our freshly-created fixture, not a historical PID.
            script = ("import app_bridge; scan=app_bridge.client.scan; "
                      f"app_bridge.client.scan=lambda:scan({host.process.pid}); app_bridge.main()")
            worker = subprocess.Popen([sys.executable, "-u", "-c", script], cwd=reference,
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
                text=True, encoding="utf-8", creationflags=subprocess.CREATE_NO_WINDOW)
            lines = queue.Queue()
            def read():
                for line in worker.stdout:
                    lines.put(line)
                lines.put(None)
            reader = threading.Thread(target=read, daemon=True)
            reader.start()
            def command(request):
                worker.stdin.write(json.dumps(request) + "\n")
                worker.stdin.flush()
                line = lines.get(timeout=15)
                self.assertIsNotNone(line, "Worker exited")
                result = json.loads(line)
                if not result["ok"]:
                    from reference import client
                    conflicts = [m["path"] for m in client.modules(host.process.pid)
                                 if m["name"].lower() in ("em64.dll", "em32.dll", "autotune_agent.dll")]
                    print(json.dumps({"fixturePid": host.process.pid, "fixtureFailure": result,
                                      "conflictingModules": conflicts}))
                    if conflicts and "hook" in result.get("error", ""):
                        self.skipTest("External helper automatically attached to owned fixture; conflict protection retained")
                self.assertTrue(result["ok"], result)
                return result
            try:
                command({"op": "status"})
                candidates = command({"op": "scan"})["candidates"]
                selected = next(c for c in candidates if c["pid"] == host.process.pid and c["compatible"])
                state = command({"op": "connect", "candidateId": selected["candidateId"]})["state"]
                deadline = time.monotonic() + 12
                while state["phase"] != "ready" and time.monotonic() < deadline:
                    time.sleep(.1)
                    state = command({"op": "status"})["state"]
                self.assertEqual(state["phase"], "ready")
                before = host.command("status")
                self.assertEqual([i["values"] for i in before["instances"]],
                                 [i["values"] for i in original["instances"]])
                result = command({"op": "apply", "connectionId": state["connectionId"],
                    "sequence": 1, "values": {"retune": .21, "flex": .32, "vibrato": .43, "humanize": .54}})
                self.assertEqual(result["state"]["delivery"]["stage"], "cached")
                expected = {i["index"]: [.21, .32, .43, .54, *i["values"][4:]]
                            for i in before["instances"] if i["class"] == 0}
                self.assertTrue(expected, before)
                after = host.wait_values(expected)
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    state = command({"op": "status"})["state"]
                    if state["delivery"]["stage"] == "submitted":
                        break
                    time.sleep(.05)
                self.assertEqual(state["delivery"]["stage"], "submitted")
                self.assertFalse(state["audioVerified"])
                disconnected = command({"op": "disconnect"})["state"]
                self.assertIsNone(disconnected["connectionId"])
                self.assertEqual([i["values"] for i in host.command("status")["instances"]],
                                 [i["values"] for i in after["instances"]])
                worker.stdin.close()
                self.assertEqual(worker.wait(timeout=5), 0)
                print(json.dumps({"fixturePid": host.process.pid, "workerPid": worker.pid,
                    "instances": state["instanceCount"], "delivery": state["delivery"],
                    "fourValues": [.21, .32, .43, .54], "workerExit": worker.returncode,
                    "audioVerified": False}))
            finally:
                if worker.poll() is None:
                    worker.kill()
                    worker.wait(timeout=5)
                if not worker.stdin.closed:
                    worker.stdin.close()
                worker.stdout.close()
                reader.join(timeout=2)
                host.close()


if __name__ == "__main__":
    unittest.main()
