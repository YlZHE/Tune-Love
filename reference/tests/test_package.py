"""Exercise the packaged worker without scanning, attaching, or starting a host."""
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import zipfile

from reference import package


class PackageTests(unittest.TestCase):
    def test_packaged_sender_runs_offline_without_workspace_or_commercial_files(self):
        with tempfile.TemporaryDirectory(prefix="autotune-sender-package-") as directory:
            root = Path(directory)
            archive = root / "source.zip"
            package.write_archive(archive, package.collect(False))
            with zipfile.ZipFile(archive) as source:
                self.assertFalse(any(name.startswith(("work/", "reference/vendor/"))
                                     or name.lower().endswith((".dll", ".exe"))
                                     for name in source.namelist()))
                source.extractall(root / "unpacked")
            result = subprocess.run(
                [sys.executable, "-m", "unittest", "reference.tests.test_sender"],
                cwd=root / "unpacked", capture_output=True, text=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_source_archive_can_run_its_own_disconnected_application_worker(self):
        with tempfile.TemporaryDirectory(prefix="autotune-source-check-") as directory:
            root = Path(directory)
            archive = root / "source.zip"
            package.write_archive(archive, package.collect(False))
            with zipfile.ZipFile(archive) as source:
                names = source.namelist()
                self.assertIn("reference/app_bridge.py", names)
                self.assertIn("reference/tests/test_app_bridge.py", names)
                self.assertNotIn("reference/build/x64/reference_agent.dll", names)
                source.extractall(root / "unpacked")
            working = root / "unpacked" / "reference"
            result = subprocess.run([sys.executable, str(working / "app_bridge.py")],
                                    cwd=working, input='{"op":"status"}\n', text=True,
                                    capture_output=True, timeout=10)
            self.assertEqual(result.returncode, 0, result.stderr)
            response = json.loads(result.stdout)
            self.assertTrue(response["ok"], response)
            self.assertEqual(response["state"]["phase"], "disconnected")
            self.assertIsNone(response["state"]["connectionId"])
            self.assertFalse(response["state"]["audioVerified"])


if __name__ == "__main__":
    unittest.main()
