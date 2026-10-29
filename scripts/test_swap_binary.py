import hashlib
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("swap_binary.sh")
SHA256SUM = '''#!/bin/sh
python3 -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest(), sys.argv[1])' "$1"
'''
# Records each call with the binary in place at that moment.
SYSTEMCTL = '''#!/bin/sh
echo "$* $(cat "$D/lg-hue-sync" 2>/dev/null)" >> "$CALLS"
if [ "$1" = start ] && [ -n "${FAIL_START_WITH:-}" ] && grep -qx "$FAIL_START_WITH" "$D/lg-hue-sync"; then
    exit 1
fi
'''


def sha(text):
    return hashlib.sha256(text.encode()).hexdigest()


class SwapTests(unittest.TestCase):
    def swap(self, new, current=None, previous=None, expected_sha=None, **environment):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            install = base / "install"
            install.mkdir()
            bin_dir = base / "bin"
            bin_dir.mkdir()
            for name, body in (("sha256sum", SHA256SUM), ("systemctl", SYSTEMCTL)):
                (bin_dir / name).write_text(body)
                (bin_dir / name).chmod(0o755)
            for name, text in (("lg-hue-sync.new", new), ("lg-hue-sync", current),
                               ("lg-hue-sync.previous", previous)):
                if text is not None:
                    (install / name).write_text(text + "\n")
            calls = base / "calls"
            calls.touch()
            env = dict(
                os.environ,
                PATH=f"{bin_dir}:{os.environ['PATH']}",
                SHA=expected_sha or sha(new + "\n"),
                LG_HUE_SYNC_DIR=str(install),
                LG_HUE_SYNC_UNIT_DIR=str(base / "run"),
                D=str(install),
                CALLS=str(calls),
                **environment,
            )
            result = subprocess.run(["sh", str(SCRIPT)], env=env,
                                    capture_output=True, text=True)
            files = {path.name: path.read_text().strip()
                     for path in install.iterdir()}
            return result.returncode, files, calls.read_text().splitlines()

    def test_upgrade_keeps_running_binary_as_previous(self):
        code, files, calls = self.swap("B", current="A", previous="old")
        self.assertEqual(code, 0)
        self.assertEqual(files, {"lg-hue-sync": "B", "lg-hue-sync.previous": "A"})
        self.assertEqual(calls, ["stop lg-hue-sync.service A",
                                 "start lg-hue-sync.service B"])

    def test_redeploy_of_same_binary_keeps_previous(self):
        code, files, _ = self.swap("B", current="B", previous="A")
        self.assertEqual(code, 0)
        self.assertEqual(files, {"lg-hue-sync": "B", "lg-hue-sync.previous": "A"})

    def test_first_install_has_no_previous(self):
        code, files, calls = self.swap("B")
        self.assertEqual(code, 0)
        self.assertEqual(files, {"lg-hue-sync": "B"})
        self.assertEqual(calls, ["stop lg-hue-sync.service ",
                                 "start lg-hue-sync.service B"])

    def test_checksum_mismatch_changes_nothing(self):
        code, files, calls = self.swap("B", current="A", expected_sha=sha("other"))
        self.assertNotEqual(code, 0)
        self.assertEqual(files, {"lg-hue-sync": "A", "lg-hue-sync.new": "B"})
        self.assertEqual(calls, [])

    def test_failed_start_restarts_previous_binary(self):
        code, files, calls = self.swap("B", current="A", FAIL_START_WITH="B")
        self.assertNotEqual(code, 0)
        self.assertEqual(files["lg-hue-sync"], "A")
        self.assertEqual(calls[-1], "start lg-hue-sync.service A")

    def test_failure_after_stop_restarts_previous_binary(self):
        # The fixture has no unit file, so the unit copy fails after the stop.
        code, files, calls = self.swap("B", current="A", INSTALL_UNIT="true")
        self.assertNotEqual(code, 0)
        self.assertEqual(files["lg-hue-sync"], "A")
        self.assertEqual(calls, ["stop lg-hue-sync.service A",
                                 "start lg-hue-sync.service A"])


if __name__ == "__main__":
    unittest.main()
