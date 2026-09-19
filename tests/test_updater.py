#!/usr/bin/env python3
"""Exercise the POSIX updater without network access or a real installation.

Every curl invocation is intercepted, every installed executable belongs to a
temporary directory, and the verified installer is a small synthetic fixture.
Run with: python3 tests/test_updater.py
"""

import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import time
import unittest


UPDATER = Path(__file__).resolve().parents[1] / "update.sh"
REPOSITORY = "https://github.com/kamilsj/vectors"

FAKE_BINARY = r'''#!/usr/bin/env python3
import pathlib, sys
binary = pathlib.Path(sys.argv[0])
version = __VERSION__
if sys.argv[1:] != ["--version"]:
    raise SystemExit("Unexpected execution of a fixture binary")
sys.stdout.write(binary.name + " " + version + "\n")
'''

FAKE_PS = r'''#!/usr/bin/env python3
import os, sys
args = sys.argv[1:]
assert args[:2] == ["-p", os.environ["UPDATER_TEST_MANAGED_PID"]], args
if args[2:] == ["-o", "command="]:
    print(os.environ.get("UPDATER_TEST_PROCESS_COMMAND", os.environ["UPDATER_TEST_INSTALL_DIR"] + "/vectors-server"))
elif args[2:] == ["-o", "lstart="]:
    print(os.environ.get("UPDATER_TEST_PROCESS_START", "Fri Sep 18 12:34:56 2026"))
else:
    raise SystemExit("Unexpected ps arguments: " + repr(args))
'''

FAKE_CURL = r'''#!/usr/bin/env python3
import json, os, pathlib, sys
args = sys.argv[1:]
root = pathlib.Path(os.environ["UPDATER_TEST_FIXTURES"])
with (root / "curl.jsonl").open("a") as log:
    log.write(json.dumps(args) + "\n")
repository = "https://github.com/kamilsj/vectors"
latest = os.environ.get("UPDATER_TEST_LATEST", "0.8.0")
url = args[-1]
if url == repository + "/releases/latest":
    kind = "latest"
elif url == repository + "/releases/download/v" + latest + "/SHA256SUMS":
    kind = "checksum"
elif url == repository + "/releases/download/v" + latest + "/install.sh":
    kind = "installer"
else:
    raise SystemExit("Unexpected URL; real network is disabled: " + url)
if os.environ.get("UPDATER_TEST_FAIL_DOWNLOAD") == kind:
    raise SystemExit(22)
if kind == "latest" and os.environ.get("UPDATER_TEST_FAIL_FIRST_CHECK") == "1" and not (root / "check-failed").exists():
    (root / "check-failed").touch()
    raise SystemExit(22)
if kind == "latest":
    sys.stdout.write(os.environ.get("UPDATER_TEST_REDIRECT", repository + "/releases/tag/v" + latest))
else:
    destination = pathlib.Path(args[args.index("--output") + 1])
    source = "SHA256SUMS" if kind == "checksum" else "install.sh"
    destination.write_bytes((root / source).read_bytes())
'''

INSTALLER = b'''#!/bin/sh
exec python3 "$UPDATER_TEST_INSTALL_HELPER" "$@"
'''

INSTALL_HELPER = r'''import json, os, pathlib, sys, time
args = sys.argv[1:]
root = pathlib.Path(os.environ["UPDATER_TEST_FIXTURES"])
with (root / "installer.jsonl").open("a") as log:
    log.write(json.dumps({"args": args, "no_start": os.environ.get("VECTORS_NO_START")}) + "\n")
assert args[:1] == ["--version"], args
assert args[2:3] == ["--install-dir"], args
assert args[4:] in (["--no-start", "--no-open"], ["--restart", "--no-open"]), args
version = args[1]
assert version.startswith("v"), version
install = pathlib.Path(args[3])
# Never act on anything outside the test-owned installation.
assert install.resolve() == pathlib.Path(os.environ["UPDATER_TEST_INSTALL_DIR"]).resolve()
behavior = os.environ.get("UPDATER_TEST_INSTALL_BEHAVIOR", "success")
if behavior == "fail":
    raise SystemExit(9)
if behavior == "hold":
    (root / "installer-entered").touch()
    deadline = time.monotonic() + 15
    while not (root / "installer-release").exists():
        if time.monotonic() > deadline:
            raise SystemExit("Fixture installer timed out")
        time.sleep(0.02)
if behavior != "unchanged":
    template = (root / "binary_template.py").read_text()
    installed_version = "0.8.1" if behavior == "wrong-version" else version[1:]
    for name in ("vectors", "vectors-server"):
        staged = install / (name + ".new")
        staged.write_text(template.replace("__VERSION__", json.dumps(installed_version)))
        staged.chmod(0o755)
        staged.replace(install / name)
        if behavior == "partial-fail":
            raise SystemExit(9)
'''


@unittest.skipUnless(os.name == "posix", "The shell updater is for POSIX systems")
class UpdaterTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="vectors-updater-test-")
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.install = self.root / "installation with spaces"
        self.state = self.root / "state"
        self.fixtures = self.root / "fixtures"
        self.mock_bin = self.root / "mock-bin"
        self.staging = self.root / "staging"
        for directory in (self.install, self.fixtures, self.mock_bin, self.staging):
            directory.mkdir()
        self.env = {
            key: value for key, value in os.environ.items()
            if not key.startswith(("VECTORS_", "UPDATER_TEST_"))
        }
        self.env.update({
            "PATH": str(self.mock_bin) + os.pathsep + os.environ.get("PATH", "/usr/bin:/bin"),
            "TMPDIR": str(self.staging),
            "VECTORS_INSTALL_DIR": str(self.install),
            "VECTORS_STATE_DIR": str(self.state),
            "UPDATER_TEST_INSTALL_DIR": str(self.install),
            "UPDATER_TEST_FIXTURES": str(self.fixtures),
            "UPDATER_TEST_INSTALL_HELPER": str(self.fixtures / "install_helper.py"),
            "UPDATER_TEST_LATEST": "0.8.0",
        })
        self.set_version("0.7.0")
        self.write_executable(self.mock_bin / "curl", FAKE_CURL)
        self.write_executable(self.mock_bin / "ps", FAKE_PS)
        (self.fixtures / "binary_template.py").write_text(FAKE_BINARY)
        (self.fixtures / "install_helper.py").write_text(INSTALL_HELPER)
        (self.fixtures / "install.sh").write_bytes(INSTALLER)
        self.checksum = hashlib.sha256(INSTALLER).hexdigest()
        self.set_checksums(self.checksum + "  install.sh\n")

    @staticmethod
    def write_executable(path, content):
        path.write_text(content)
        path.chmod(0o755)

    def set_version(self, version, server_version=None):
        for name, current in (("vectors", version), ("vectors-server", version if server_version is None else server_version)):
            self.write_executable(self.install / name, FAKE_BINARY.replace("__VERSION__", json.dumps(current)))

    def installed_version(self, name="vectors"):
        return subprocess.check_output([str(self.install / name), "--version"], text=True).strip()

    def set_checksums(self, content):
        (self.fixtures / "SHA256SUMS").write_text(content)

    def run_updater(self, *args, **environment):
        return subprocess.run(
            ["sh", str(UPDATER), *args], env={**self.env, **environment},
            text=True, capture_output=True, timeout=15,
        )

    def snapshot(self):
        return {
            str(path.relative_to(self.install)): path.read_bytes()
            for path in self.install.rglob("*") if path.is_file()
        }

    def records(self, name):
        path = self.fixtures / (name + ".jsonl")
        return [json.loads(line) for line in path.read_text().splitlines()] if path.exists() else []

    def clear_logs(self):
        for name in ("curl", "installer"):
            (self.fixtures / (name + ".jsonl")).unlink(missing_ok=True)

    def assert_success(self, result):
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)

    def assert_clean(self):
        self.assertFalse((self.install / ".vectors-update.lock").exists())
        self.assertEqual(list(self.staging.iterdir()), [])

    def assert_failure_without_install(self, result, before):
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertEqual(self.records("installer"), [])
        self.assertEqual(self.snapshot(), before)
        self.assert_clean()

    def assert_rejected(self, *args, **environment):
        before = self.snapshot()
        result = self.run_updater(*args, **environment)
        self.assert_failure_without_install(result, before)
        return result

    def managed_server(self, authenticated=False):
        # The process is our child; the fixture installer never signals it.
        process = subprocess.Popen(["sleep", "60"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.addCleanup(self.stop_process, process)
        self.state.mkdir(exist_ok=True)
        (self.state / "server.pid").write_text(str(process.pid) + "\n")
        self.env["UPDATER_TEST_MANAGED_PID"] = str(process.pid)
        (self.state / "server.identity").write_text(str(process.pid) + "\nFri Sep 18 12:34:56 2026\n")
        (self.state / "server.config").write_text("\n".join([
            "vectors-installer-config-v1", "127.0.0.1:9999", "data",
            str(self.root / "data"), "30", str(self.state / "server.log"),
            "0.7.0", "1" if authenticated else "0", "30", "",
        ]))
        return process

    @staticmethod
    def stop_process(process):
        if process.poll() is None:
            process.terminate()
        try:
            process.wait(timeout=3)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=3)

    @staticmethod
    def wait_for(path, process):
        deadline = time.monotonic() + 5
        while not path.exists():
            if process.poll() is not None:
                raise AssertionError("Updater exited before reaching fixture installer")
            if time.monotonic() >= deadline:
                raise AssertionError("Updater did not reach fixture installer")
            time.sleep(0.02)

    def test_equal_or_older_latest_never_installs_or_downgrades(self):
        for installed, latest in (("0.7.0", "0.7.0"), ("1.0.0", "0.99.99"), ("0.10.0", "0.9.99")):
            with self.subTest(installed=installed, latest=latest):
                self.clear_logs()
                self.set_version(installed)
                before = self.snapshot()
                result = self.run_updater(UPDATER_TEST_LATEST=latest)
                self.assert_success(result)
                self.assertIn("up to date", result.stdout)
                self.assertEqual(len(self.records("curl")), 1)
                self.assertEqual(self.records("installer"), [])
                self.assertEqual(self.snapshot(), before)
                self.assert_clean()

    def test_newer_release_uses_verified_pinned_installer_and_explicit_flags(self):
        result = self.run_updater("--install-dir", str(self.install))
        self.assert_success(result)
        self.assertIn("Updated vectors to 0.8.0", result.stdout)
        self.assertEqual(self.records("installer"), [{
            "args": ["--version", "v0.8.0", "--install-dir", str(self.install), "--no-start", "--no-open"],
            "no_start": None,
        }])
        calls = self.records("curl")
        self.assertEqual([args[-1] for args in calls], [
            REPOSITORY + "/releases/latest",
            REPOSITORY + "/releases/download/v0.8.0/SHA256SUMS",
            REPOSITORY + "/releases/download/v0.8.0/install.sh",
        ])
        for args in calls:
            self.assertEqual(args[args.index("--proto") + 1], "=https")
            self.assertEqual(args[args.index("--proto-redir") + 1], "=https")
            self.assertIn("--fail", args)
            self.assertIn("--max-time", args)
        for name in ("vectors", "vectors-server"):
            self.assertEqual(self.installed_version(name), name + " 0.8.0")
        self.assert_clean()

    def test_check_is_read_only_even_with_existing_update_lock(self):
        lock = self.install / ".vectors-update.lock"
        lock.mkdir()
        (lock / "owner").write_text("another process owns this\n")
        before = self.snapshot()
        self.install.chmod(0o555)
        try:
            result = self.run_updater("--check")
        finally:
            self.install.chmod(0o755)
        self.assert_success(result)
        self.assertIn("Update available", result.stdout)
        self.assertEqual(self.snapshot(), before)
        self.assertFalse(self.state.exists())
        self.assertEqual(self.records("installer"), [])
        self.assertEqual(len(self.records("curl")), 1)
        self.assertEqual(list(self.staging.iterdir()), [])

    def test_installed_and_latest_versions_are_strict_stable_semver(self):
        invalid = ("1.2", "1.2.3.4", "01.2.3", "1.2.3-rc1", "1.2.3+build", "1.2.-1", "4294967296.0.0", "1.2.3\n2.3.4", "1.2. 3")
        for version in invalid:
            for source in ("installed", "latest"):
                with self.subTest(version=repr(version), source=source):
                    self.clear_logs()
                    self.set_version(version if source == "installed" else "0.7.0")
                    before = self.snapshot()
                    result = self.run_updater(UPDATER_TEST_LATEST=version if source == "latest" else "0.8.0")
                    self.assert_failure_without_install(result, before)
                    self.assertIn("stable", result.stderr)

    def test_numeric_version_comparison_accepts_multi_digit_components(self):
        self.set_version("0.9.99")
        result = self.run_updater(UPDATER_TEST_LATEST="0.10.0")
        self.assert_success(result)
        self.assertEqual(self.installed_version(), "vectors 0.10.0")
        self.assert_clean()

    def test_unofficial_latest_redirect_cannot_select_an_installer(self):
        for target in ("https://example.invalid/releases/tag/v9.0.0", REPOSITORY + "/releases/tag/v0.8.0?next=evil", REPOSITORY + "-evil/releases/tag/v9.0.0"):
            with self.subTest(target=target):
                self.clear_logs()
                self.assert_rejected(UPDATER_TEST_REDIRECT=target)
                self.assertEqual(len(self.records("curl")), 1)

    def test_each_failed_download_leaves_the_installation_unchanged(self):
        for kind in ("latest", "checksum", "installer"):
            with self.subTest(kind=kind):
                self.clear_logs()
                self.assert_rejected(UPDATER_TEST_FAIL_DOWNLOAD=kind)

    def test_duplicate_missing_malformed_or_wrong_checksum_never_executes_installer(self):
        for checksum in (
            self.checksum + "  install.sh\n" + self.checksum + " *install.sh\n",
            self.checksum + "  other-file\n", "bad  install.sh\n",
            "g" * 64 + "  install.sh\n", "0" * 64 + "  install.sh\n",
        ):
            with self.subTest(checksum=checksum):
                self.clear_logs()
                self.set_checksums(checksum)
                self.assert_rejected()

    def test_modified_installer_is_rejected_before_execution(self):
        (self.fixtures / "install.sh").write_bytes(INSTALLER + b"# unexpected modification\n")
        self.assert_rejected()

    def test_uppercase_binary_checksum_entry_is_supported(self):
        self.set_checksums(self.checksum.upper() + " *install.sh\n")
        self.assert_success(self.run_updater())
        self.assert_clean()

    def test_oversized_metadata_is_rejected_even_if_curl_ignores_size_limit(self):
        self.set_checksums("x" * (1048576 + 1))
        self.assert_rejected()

    def test_stopped_server_stays_stopped(self):
        self.state.mkdir()
        completed = subprocess.Popen(["sh", "-c", "exit 0"])
        completed.wait(timeout=3)
        (self.state / "server.pid").write_text(str(completed.pid) + "\n")
        self.assert_success(self.run_updater())
        self.assertIn("--no-start", self.records("installer")[0]["args"])
        self.assertNotIn("--restart", self.records("installer")[0]["args"])
        self.assert_clean()

    def test_no_start_never_restarts_a_running_server_or_requires_its_secret(self):
        process = self.managed_server(authenticated=True)
        for args, environment in ((["--no-start"], {}), ([], {"VECTORS_NO_START": "1"})):
            with self.subTest(args=args, environment=environment):
                self.clear_logs()
                self.set_version("0.7.0")
                self.assert_success(self.run_updater(*args, **environment))
                self.assertIn("--no-start", self.records("installer")[0]["args"])
                self.assertNotIn("--restart", self.records("installer")[0]["args"])
                self.assertIsNone(process.poll())
                self.assert_clean()

    def test_managed_authenticated_server_requires_token_before_downloading_installer(self):
        self.managed_server(authenticated=True)
        result = self.assert_rejected()
        self.assertIn("VECTORS_API_TOKEN", result.stderr)
        self.assertEqual(len(self.records("curl")), 1)

    def test_running_managed_server_uses_restart_with_existing_auth_environment(self):
        self.managed_server(authenticated=True)
        self.assert_success(self.run_updater(VECTORS_API_TOKEN="fixture-existing-token"))
        install = self.records("installer")[0]
        self.assertIn("--restart", install["args"])
        self.assertNotIn("--no-start", install["args"])
        self.assertEqual(install["no_start"], "0")
        self.assert_clean()

    def test_unrelated_or_reused_managed_pid_is_rejected_before_installation(self):
        process = self.managed_server()
        for environment in (
            {"UPDATER_TEST_PROCESS_COMMAND": "/another/installation/vectors-server"},
            {"UPDATER_TEST_PROCESS_START": "Fri Sep 18 12:34:57 2026"},
        ):
            with self.subTest(environment=environment):
                self.clear_logs()
                self.assert_rejected(**environment)
                self.assertEqual(len(self.records("curl")), 1)
                self.assertIsNone(process.poll())
        (self.state / "server.identity").unlink()
        self.clear_logs()
        self.assert_rejected()
        self.assertIsNone(process.poll())

    def test_invalid_flags_intervals_and_pinned_version_fail_before_network(self):
        cases = [
            (["--check", "--watch"], {}), (["--unknown"], {}),
            (["--interval"], {}), (["--install-dir"], {}), (["--install-dir", ""], {}),
            ([], {"VECTORS_VERSION": "v0.7.0"}),
        ]
        cases += [(["--check", "--interval", value], {}) for value in ("", "59", "604801", "-1", "1.5", "abc", "999999999999999999999")]
        for args, environment in cases:
            with self.subTest(args=args, environment=environment):
                self.clear_logs()
                self.assert_rejected(*args, **environment)
                self.assertEqual(self.records("curl"), [])
        for boundary in ("60", "604800"):
            with self.subTest(boundary=boundary):
                self.assert_success(self.run_updater("--check", "--interval", boundary))
        self.assert_clean()

    def test_mismatched_or_symlinked_installed_pair_is_not_replaced(self):
        self.set_version("0.7.0", "0.6.0")
        self.assert_rejected()
        self.assertEqual(self.records("curl"), [])
        self.set_version("0.7.0")
        original = self.install / "vectors"
        target = self.root / "package-manager-vectors"
        original.rename(target)
        original.symlink_to(target)
        self.assert_rejected()
        self.assertTrue(original.is_symlink())
        self.assertEqual(self.records("curl"), [])

    def test_installer_failure_or_false_success_is_reported_and_cleans_lock(self):
        for behavior in ("fail", "unchanged", "partial-fail", "wrong-version"):
            with self.subTest(behavior=behavior):
                self.clear_logs()
                before = self.snapshot()
                result = self.run_updater(UPDATER_TEST_INSTALL_BEHAVIOR=behavior)
                self.assertNotEqual(result.returncode, 0)
                self.assertEqual(len(self.records("installer")), 1)
                self.assertEqual(self.snapshot(), before)
                self.assert_clean()

    def test_concurrent_updater_cannot_enter_installer_or_remove_owned_lock(self):
        process = subprocess.Popen(
            ["sh", str(UPDATER)], env={**self.env, "UPDATER_TEST_INSTALL_BEHAVIOR": "hold"},
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
        )
        try:
            self.wait_for(self.fixtures / "installer-entered", process)
            lock = self.install / ".vectors-update.lock"
            owner = (lock / "owner").read_bytes()
            second = self.run_updater()
            self.assertNotEqual(second.returncode, 0)
            self.assertIn("another updater", second.stderr)
            self.assertEqual((lock / "owner").read_bytes(), owner)
            self.assertEqual(len(self.records("installer")), 1)
            (self.fixtures / "installer-release").touch()
            stdout, stderr = process.communicate(timeout=8)
            self.assertEqual(process.returncode, 0, stdout + stderr)
            self.assert_clean()
        finally:
            if process.poll() is None:
                os.killpg(process.pid, signal.SIGKILL)
            process.communicate(timeout=3)

    def test_interrupt_during_installer_keeps_transaction_locked_until_it_finishes(self):
        process = subprocess.Popen(
            ["sh", str(UPDATER)], env={**self.env, "UPDATER_TEST_INSTALL_BEHAVIOR": "hold"},
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
        )
        try:
            self.wait_for(self.fixtures / "installer-entered", process)
            os.killpg(process.pid, signal.SIGTERM)
            # Transaction protection must retain its lock even when its caller
            # is interrupted; otherwise another updater could replace its pair.
            second = self.run_updater()
            self.assertNotEqual(second.returncode, 0)
            self.assertIn("another updater", second.stderr)
            self.assertEqual(len(self.records("installer")), 1)
            (self.fixtures / "installer-release").touch()
            process.communicate(timeout=8)
            self.assertEqual(self.installed_version(), "vectors 0.8.0")
            self.assertEqual(self.installed_version("vectors-server"), "vectors-server 0.8.0")
            self.assert_clean()
        finally:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=3)

    def test_watch_waits_between_failed_checks_and_interrupt_stops_its_sleep(self):
        self.set_version("0.8.0")
        self.write_executable(self.mock_bin / "sleep", '''#!/usr/bin/env python3
import json, os, pathlib, sys, time
root = pathlib.Path(os.environ["UPDATER_TEST_FIXTURES"])
count_file = root / "sleep-count"
count = int(count_file.read_text()) + 1 if count_file.exists() else 1
count_file.write_text(str(count))
(root / ("sleep-" + str(count) + ".json")).write_text(json.dumps({"pid": os.getpid(), "args": sys.argv[1:]}))
deadline = time.monotonic() + 15
while not (root / ("sleep-release-" + str(count))).exists():
    if time.monotonic() > deadline:
        raise SystemExit("Fixture sleep timed out")
    time.sleep(0.02)
''')
        process = subprocess.Popen(
            ["sh", str(UPDATER), "--watch", "--interval", "123"],
            env={**self.env, "UPDATER_TEST_FAIL_FIRST_CHECK": "1"},
            text=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
        )
        try:
            first_marker = self.fixtures / "sleep-1.json"
            self.wait_for(first_marker, process)
            first = json.loads(first_marker.read_text())
            self.assertEqual(first["args"], ["123"])
            self.assertEqual(len(self.records("curl")), 1)
            self.assert_clean()
            time.sleep(0.15)
            self.assertEqual(len(self.records("curl")), 1, "watch must wait before retrying")
            (self.fixtures / "sleep-release-1").touch()
            second_marker = self.fixtures / "sleep-2.json"
            self.wait_for(second_marker, process)
            second = json.loads(second_marker.read_text())
            self.assertEqual(second["args"], ["123"])
            self.assertEqual(len(self.records("curl")), 2)
            with self.assertRaises(ProcessLookupError):
                os.kill(first["pid"], 0)
            process.terminate()
            process.communicate(timeout=5)
            self.assertNotEqual(process.returncode, 0)
            with self.assertRaises(ProcessLookupError):
                os.kill(second["pid"], 0)
            self.assertEqual(self.records("installer"), [])
            self.assert_clean()
        finally:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.communicate(timeout=3)


if __name__ == "__main__":
    unittest.main(verbosity=2)
