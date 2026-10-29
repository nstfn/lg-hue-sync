import json
import os
from pathlib import Path
import stat
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("provision_luna.sh")
NAME = "org.webosbrew.lg-hue-sync.json"
ROOTS = ("/var/luna-service2", "/var/luna-service2-dev")
SERVICES = (
    "com.webos.service.capture",
    "com.webos.service.capture.client*",
    "com.webos.rm.client.*",
    "com.webos.service.capturepermission",
    "org.webosbrew.lg-hue-sync*",
)
# The Luna names and permissions must stay exactly these until a
# least-privilege version is checked on a TV.
ROLE = {
    "exeName": "/var/home/root/lg-hue-sync/lg-hue-sync",
    "type": "privileged",
    "allowedNames": list(SERVICES),
    "permissions": [
        {"service": service, "inbound": ["*"], "outbound": ["*"]}
        for service in SERVICES
    ],
}
PERMISSIONS = {
    "org.webosbrew.lg-hue-sync*": ["all"],
    "com.webos.service.capture.client*": ["all"],
    "com.webos.service.capture": ["all"],
    "com.webos.service.capturepermission": ["all"],
    "com.webos.rm.client.*": ["all"],
}
LS_CONTROL = '#!/bin/sh\necho "ls-control $*" >> "$CALLS"\n'
# Runs the "remote" command locally: the last argument is the remote command.
SSH = '#!/bin/sh\necho "ssh $*" >> "$CALLS"\nfor last; do :; done\nexec sh -c "$last"\n'


def manifest(root):
    return {
        "id": "org.webosbrew.lg-hue-sync",
        "version": "1.0.0",
        "roleFiles": [f"{root}/roles.d/{NAME}"],
        "clientPermissionFiles": [f"{root}/client-permissions.d/{NAME}"],
        "serviceFiles": [],
        "apiPermissionFiles": [],
    }


class ProvisionTests(unittest.TestCase):
    def provision(self, target):
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            prefix = base / "root"
            bin_dir = base / "bin"
            bin_dir.mkdir()
            for name, body in (("ls-control", LS_CONTROL), ("ssh", SSH)):
                (bin_dir / name).write_text(body)
                (bin_dir / name).chmod(0o755)
            calls = base / "calls"
            env = dict(
                os.environ,
                PATH=f"{bin_dir}:{os.environ['PATH']}",
                LUNA_PREFIX=str(prefix),
                CALLS=str(calls),
            )
            subprocess.run(["bash", str(SCRIPT), target], env=env, check=True,
                           capture_output=True, text=True)
            files = {
                str(path.relative_to(prefix)): (
                    path.read_text(), stat.S_IMODE(path.stat().st_mode))
                for path in prefix.rglob("*") if path.is_file()
            }
            return files, calls.read_text()

    def check_files(self, files):
        expected = {}
        for root in ROOTS:
            expected[f"{root[1:]}/manifests.d/{NAME}"] = manifest(root)
            expected[f"{root[1:]}/roles.d/{NAME}"] = ROLE
            expected[f"{root[1:]}/client-permissions.d/{NAME}"] = PERMISSIONS
        # No temporary file is left next to the targets.
        self.assertEqual(set(files), set(expected))
        for path, (text, mode) in files.items():
            self.assertEqual(json.loads(text), expected[path], path)
            self.assertEqual(mode, 0o644, path)

    def test_local_path_writes_exact_files_and_scans(self):
        files, calls = self.provision("local")
        self.check_files(files)
        self.assertEqual(calls, "ls-control scan-services\n")

    def test_remote_path_runs_the_same_function(self):
        files, calls = self.provision("192.0.2.10")
        self.check_files(files)
        self.assertIn("ssh -p 22 ", calls)
        self.assertIn("root@192.0.2.10 bash -s -- local", calls)
        self.assertTrue(calls.endswith("ls-control scan-services\n"))

    def test_no_fixed_tmp_name(self):
        self.assertNotIn("/tmp/", SCRIPT.read_text())


if __name__ == "__main__":
    unittest.main()
