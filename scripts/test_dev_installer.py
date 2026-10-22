#!/usr/bin/env python3
"""Exercise development commands, installation, and MCP setup in isolated directories."""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]

FAKE_TOOL = r'''
import json, os, pathlib, shutil, sys
tool = pathlib.Path(sys.argv[0]).name
args = sys.argv[1:]
with open(os.environ["SONAR_TEST_LOG"], "a") as stream:
    stream.write(json.dumps({"tool": tool, "args": args, "cwd": os.getcwd()}) + "\n")
if tool == "cargo":
    if "--version" in args:
        if os.environ.get("SONAR_TEST_MISSING_COMPONENT") == args[0]:
            sys.exit(1)
        print("cargo component 1.0")
    elif args[0] == "build":
        if os.environ.get("SONAR_TEST_BUILD_FAIL"):
            sys.exit(19)
        profile = "release" if "--release" in args else "debug"
        output = pathlib.Path(args[args.index("--target-dir") + 1]) / profile / "sonar"
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(pathlib.Path(__file__).read_text() + "\n# " + os.environ.get("SONAR_TEST_REV", "first") + "\n")
        output.chmod(0o755)
    else:
        sys.exit(int(os.environ.get("SONAR_TEST_" + args[0].upper() + "_EXIT", "0")))
elif tool == "sonar":
    if args == ["--version"]:
        print("foreign 1" if os.environ.get("SONAR_TEST_BAD_ID") else "sonar 0.1.0")
    elif args and args[0] == "--db":
        operation = args[2]
        if os.environ.get("SONAR_TEST_FAIL_OPERATION") == operation:
            sys.exit(23)
        if operation == "index":
            pathlib.Path(args[1]).write_text("index state")
        print("{}")
    else:
        print(json.dumps({"args": args, "cwd": os.getcwd()}))
        sys.exit(int(os.environ.get("SONAR_TEST_RUN_EXIT", "0")))
elif tool == "python3":
    if args and args[0] == "-c":
        sys.exit(int(os.environ.get("SONAR_TEST_PYTHON_VERSION_EXIT", "0")))
    sys.exit(int(os.environ.get("SONAR_TEST_PYTHON_TEST_EXIT", "0")))
elif tool == "rustc":
    print("rustc 1.0")
elif tool == "codex":
    state = pathlib.Path(os.environ["SONAR_TEST_CODEX_STATE"])
    if args[1] == "list":
        if os.environ.get("SONAR_TEST_BAD_CONFIG"):
            sys.exit(31)
        print("[]")
    elif args[1] == "get":
        sys.exit(0 if state.exists() else 1)
    elif args[1] == "add":
        state.write_text(json.dumps(args))
'''


class DevInstallerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="sonar dev $literal ")
        self.addCleanup(self.temp.cleanup)
        self.work = Path(self.temp.name)
        self.checkout = self.work / "checkout"
        self.checkout.mkdir()
        self.script = self.checkout / "dev.sh"
        shutil.copy2(ROOT / "dev.sh", self.script)
        (self.checkout / "Cargo.toml").write_text("[package]\nname='sonar'\n")
        (self.checkout / "Cargo.lock").write_text("# test lockfile\n")
        self.bin = self.work / "installed bin"
        self.target = self.work / "build cache"
        self.repo = self.work / "sample repo"
        self.repo.mkdir()
        self.db = self.work / "index directory/sonar.sqlite"
        self.model = self.work / "local model"
        self.model.mkdir()
        self.log = self.work / "calls.jsonl"
        self.state = self.work / "codex-entry.json"
        fake = self.work / "commands"
        self.commands = fake
        fake.mkdir()
        for name in ["cargo", "codex", "rust-analyzer", "rustc", "python3", "node"]:
            path = fake / name
            path.write_text(f"#!{sys.executable}\n" + FAKE_TOOL)
            path.chmod(0o755)
        self.env = dict(os.environ, PATH=str(fake) + os.pathsep + os.environ["PATH"],
                        SONAR_DEV_INSTALL_DIR=str(self.bin), CARGO_TARGET_DIR=str(self.target),
                        SONAR_PYTHON=str(fake / "python3"),
                        SONAR_TEST_LOG=str(self.log), SONAR_TEST_CODEX_STATE=str(self.state))

    def run_dev(self, *args, success=True, **env):
        result = subprocess.run([str(self.script), *map(str, args)], cwd=self.work,
                                env=dict(self.env, **env), capture_output=True, text=True)
        if success:
            self.assertEqual(result.returncode, 0, result.stderr + result.stdout)
        else:
            self.assertNotEqual(result.returncode, 0, result.stdout)
        return result

    def calls(self, tool=None):
        rows = [json.loads(line) for line in self.log.read_text().splitlines()] if self.log.exists() else []
        return [r for r in rows if tool is None or r["tool"] == tool]

    def install(self):
        self.run_dev("install")

    def test_install_reinstall_failure_and_uninstall_preserve_state(self):
        self.install()
        binary = self.bin / "sonar"
        first = binary.read_bytes()
        self.assertTrue(os.access(binary, os.X_OK))
        cargo = self.calls("cargo")[0]
        self.assertEqual(cargo["cwd"], str(self.checkout))
        self.assertIn("--locked", cargo["args"])
        self.assertIn("--release", cargo["args"])
        self.run_dev("install", "--prod", success=False)
        self.assertEqual(len(self.calls("cargo")), 1)
        self.run_dev("reinstall", success=False, SONAR_TEST_BUILD_FAIL="1")
        self.assertEqual(binary.read_bytes(), first)
        self.run_dev("reinstall", "--prod", SONAR_TEST_REV="second")
        self.assertNotEqual(binary.read_bytes(), first)
        self.db.parent.mkdir()
        self.db.write_text("keep database")
        self.state.write_text("keep configuration")
        self.run_dev("uninstall")
        self.assertFalse(binary.exists())
        self.assertEqual(self.db.read_text(), "keep database")
        self.assertEqual(self.state.read_text(), "keep configuration")
        self.run_dev("uninstall")

    def test_invalid_invocations_do_not_build_or_register(self):
        for args in [("install", "--extra"), ("install", "--prod", "--extra"),
                     ("reinstall", "--extra"), ("uninstall", "--prod"),
                     ("doctor", "--extra"), ("develop", "--extra"),
                     ("check", "--fix"), ("fmt", "--extra"), ("build", "--extra"),
                     ("help", "--extra"),
                     ("setup",), ("unknown",), ("",)]:
            self.run_dev(*args, success=False)
        self.assertEqual(self.calls(), [])
        self.run_dev("--help")
        self.run_dev("setup", "--help")

    def test_help_and_command_help_do_not_require_tools_or_create_files(self):
        for args in [(), ("help",), ("--help",), ("-h",)]:
            result = self.run_dev(*args, SONAR_DEV_INSTALL_DIR="/", CARGO_TARGET_DIR="relative")
            self.assertIn("doctor", result.stdout)
            self.assertIn("develop", result.stdout)
        for command in ["doctor", "install", "reinstall", "uninstall", "develop",
                        "check", "fmt", "build", "setup", "refresh"]:
            self.run_dev(command, "--help")
        self.assertEqual(self.calls(), [])
        self.assertFalse(self.bin.exists())
        self.assertFalse(self.target.exists())

    def test_legacy_install_and_reinstall_without_prior_install(self):
        self.run_dev("install", "--prod")
        self.run_dev("uninstall")
        self.run_dev("reinstall")
        self.assertTrue((self.bin / "sonar").is_file())
        self.assertTrue(all("--release" in call["args"] for call in self.calls("cargo")))

    def test_develop_fetches_locked_dependencies_in_checkout(self):
        self.run_dev("develop")
        self.assertEqual(self.calls("cargo"), [
            {"tool": "cargo", "args": ["fetch", "--locked"], "cwd": str(self.checkout)}])
        self.assertFalse(self.bin.exists())
        self.assertFalse(self.target.exists())

    def test_build_creates_release_without_installing_or_registering(self):
        self.run_dev("build")
        self.assertTrue((self.target / "release/sonar").is_file())
        self.assertFalse(self.bin.exists())
        self.assertFalse(self.state.exists())
        call = self.calls("cargo")[0]
        self.assertEqual(call["cwd"], str(self.checkout))
        self.assertIn("--release", call["args"])
        self.assertIn("--locked", call["args"])

    def test_run_preserves_arguments_caller_directory_and_exit_status(self):
        arguments = ["--help", "with spaces", "", 'a "quoted" value',
                     "$(not-a-command); & | *", "café", "trailing slash\\"]
        result = self.run_dev("run", *arguments, success=False, SONAR_TEST_RUN_EXIT="23")
        self.assertEqual(result.returncode, 23)
        self.assertEqual(json.loads(result.stdout), {"args": arguments, "cwd": str(self.work)})
        build = self.calls("cargo")[0]
        self.assertEqual(build["cwd"], str(self.checkout))
        self.assertNotIn("--release", build["args"])
        self.assertIn("--locked", build["args"])
        self.assertTrue((self.target / "debug/sonar").is_file())
        self.assertFalse(self.bin.exists())

    def test_run_does_not_execute_stale_binary_after_failed_build(self):
        self.run_dev("run", "--help")
        self.log.write_text("")
        result = self.run_dev("run", "--help", success=False, SONAR_TEST_BUILD_FAIL="1")
        self.assertEqual(result.returncode, 19)
        self.assertEqual(self.calls("sonar"), [])

    def test_check_runs_both_checks_without_fixing_and_propagates_failures(self):
        for fmt_status, clippy_status in [(0, 0), (7, 0), (0, 9), (7, 9)]:
            with self.subTest(fmt=fmt_status, clippy=clippy_status):
                self.log.write_text("")
                result = self.run_dev("check", success=not (fmt_status or clippy_status),
                                      SONAR_TEST_FMT_EXIT=str(fmt_status),
                                      SONAR_TEST_CLIPPY_EXIT=str(clippy_status))
                self.assertEqual(result.returncode, fmt_status or clippy_status)
                calls = self.calls("cargo")
                self.assertEqual(calls[0]["args"], ["fmt", "--all", "--", "--check"])
                self.assertEqual(calls[1]["args"], ["clippy", "--all-targets", "--locked",
                                 "--target-dir", str(self.target), "--", "-D", "warnings"])
                self.assertTrue(all(call["cwd"] == str(self.checkout) for call in calls))

    def test_fmt_formats_checkout(self):
        self.run_dev("fmt")
        self.assertEqual(self.calls("cargo"), [
            {"tool": "cargo", "args": ["fmt", "--all"], "cwd": str(self.checkout)}])

    def test_test_forwards_cargo_arguments_and_runs_installer_suite(self):
        arguments = ["--lib", "name with spaces", "--", "--nocapture"]
        self.run_dev("test", *arguments)
        cargo = self.calls("cargo")[0]
        self.assertEqual(cargo["args"], ["test", "--locked", "--target-dir", str(self.target), *arguments])
        self.assertEqual(cargo["cwd"], str(self.checkout))
        python = self.calls("python3")[-1]
        self.assertEqual(python["args"], ["-B", "-m", "unittest", "discover", "-s", "scripts", "-p", "test_*.py"])
        self.assertEqual(python["cwd"], str(self.checkout))

    def test_test_propagates_failures_from_either_suite(self):
        for rust_status, python_status in [(0, 0), (8, 0), (0, 11), (8, 11)]:
            with self.subTest(rust=rust_status, python=python_status):
                self.log.write_text("")
                result = self.run_dev("test", success=not (rust_status or python_status),
                                      SONAR_TEST_TEST_EXIT=str(rust_status),
                                      SONAR_TEST_PYTHON_TEST_EXIT=str(python_status))
                self.assertEqual(result.returncode, rust_status or python_status)
                self.assertEqual(self.calls("python3")[-1]["args"][0], "-B")

    def test_test_rejects_missing_or_old_python_before_running_cargo(self):
        self.run_dev("test", success=False, SONAR_PYTHON=str(self.work / "missing python"))
        self.run_dev("test", success=False, SONAR_TEST_PYTHON_VERSION_EXIT="1")
        self.assertEqual(self.calls("cargo"), [])

    def test_test_resolves_relative_python_override_before_changing_directory(self):
        self.run_dev("test", SONAR_PYTHON="commands/python3")
        self.assertEqual(self.calls("python3")[-1]["cwd"], str(self.checkout))

    def test_doctor_only_checks_tools_and_accepts_missing_optional_integrations(self):
        for name in ["codex", "rust-analyzer", "node"]:
            (self.commands / name).unlink()
        # Limit PATH so globally installed integrations cannot satisfy the check.
        (self.commands / "bash").symlink_to(shutil.which("bash"))
        (self.commands / "dirname").symlink_to(shutil.which("dirname"))
        result = self.run_dev("doctor", PATH=str(self.commands))
        self.assertIn("codex (optional): not found", result.stdout)
        self.assertIn("sonar on PATH: not installed or not on PATH", result.stdout)
        self.assertEqual([call["args"] for call in self.calls("cargo")],
                         [["--version"], ["fmt", "--version"], ["clippy", "--version"]])
        self.assertTrue(all(call["cwd"] == str(self.checkout) for call in self.calls("cargo")))
        self.assertFalse(self.target.exists())
        self.assertFalse(self.bin.exists())
        self.assertFalse(self.state.exists())

    def test_doctor_fails_for_missing_lockfile_or_component(self):
        self.run_dev("doctor", success=False, SONAR_TEST_MISSING_COMPONENT="clippy")
        (self.checkout / "Cargo.lock").unlink()
        result = self.run_dev("doctor", success=False)
        self.assertIn("Cargo.lock: MISSING", result.stdout)

    def test_destination_symlinks_and_foreign_executables_are_preserved(self):
        self.bin.mkdir()
        foreign = self.work / "foreign"
        foreign.write_text("keep")
        (self.bin / "sonar").symlink_to(foreign)
        for args in [("install", "--prod"), ("reinstall",), ("uninstall",)]:
            self.run_dev(*args, success=False)
        self.assertEqual(foreign.read_text(), "keep")
        self.assertTrue((self.bin / "sonar").is_symlink())
        (self.bin / "sonar").unlink()
        self.install()
        before = (self.bin / "sonar").read_bytes()
        self.run_dev("uninstall", success=False, SONAR_TEST_BAD_ID="1")
        self.assertEqual((self.bin / "sonar").read_bytes(), before)

    def test_install_directory_and_target_validation(self):
        self.run_dev("install", "--prod", success=False, SONAR_DEV_INSTALL_DIR="/")
        self.run_dev("install", "--prod", success=False, CARGO_TARGET_DIR="relative")
        self.bin.symlink_to(self.repo, target_is_directory=True)
        self.run_dev("install", "--prod", success=False)
        self.assertEqual(self.calls(), [])

    def test_setup_quotes_paths_and_registers_after_all_preparation(self):
        self.install()
        self.log.write_text("")
        self.run_dev("setup", self.repo.name, "--db", self.db.relative_to(self.work),
                     "--name", "sonar-example", "--resolve", "--model", self.model.name,
                     "--resources", "yaml")
        operations = [r["args"][2] for r in self.calls("sonar") if r["args"][0] == "--db"]
        self.assertEqual(operations, ["index", "index-navigation", "resolve", "embed"])
        index = next(r["args"] for r in self.calls("sonar") if "index" in r["args"])
        self.assertEqual(index, ["--db", str(self.db), "index", str(self.repo), "--resources", "yaml"])
        registered = json.loads(self.state.read_text())
        self.assertEqual(registered, ["mcp", "add", "sonar-example", "--", str(self.bin / "sonar"),
                                     "--db", str(self.db), "mcp", "--model", str(self.model)])
        self.assertEqual(self.calls()[-1]["args"], registered)
        self.assertEqual(Path(str(self.db) + ".index-navigation.json").read_text(), "{}\n")

    def test_default_name_and_database_are_checkout_relative(self):
        self.install()
        self.run_dev("setup", self.repo)
        registered = json.loads(self.state.read_text())
        self.assertEqual(registered[2], "sonar-sample-repo")
        self.assertEqual(registered[registered.index("--db") + 1],
                         str(self.checkout / ".sonar/mcp/sonar-sample-repo.sqlite"))

    def test_refresh_does_not_invoke_codex(self):
        self.install()
        self.state.write_text("existing settings")
        self.log.write_text("")
        self.run_dev("refresh", self.repo, "--db", self.db, "--model", self.model, "--resolve")
        self.assertEqual(self.calls("codex"), [])
        self.assertEqual(self.state.read_text(), "existing settings")

    def test_failed_preparation_never_registers(self):
        self.install()
        for operation in ["index", "index-navigation", "resolve", "embed"]:
            self.log.write_text("")
            self.run_dev("setup", self.repo, "--db", self.db, "--model", self.model, "--resolve",
                         success=False, SONAR_TEST_FAIL_OPERATION=operation)
            self.assertFalse(self.state.exists())
            self.assertFalse(any(r["args"][1] == "add" for r in self.calls("codex")))

    def test_existing_entry_needs_explicit_replace_and_bad_config_stops_early(self):
        self.install()
        self.state.write_text("existing settings")
        self.run_dev("setup", self.repo, "--db", self.db, success=False)
        self.assertFalse(self.db.exists())
        self.assertEqual(self.state.read_text(), "existing settings")
        self.run_dev("setup", self.repo, "--db", self.db, "--replace")
        self.assertIn("mcp", json.loads(self.state.read_text()))
        self.db.unlink()
        self.run_dev("setup", self.repo, "--db", self.db, "--replace",
                     success=False, SONAR_TEST_BAD_CONFIG="1")
        self.assertFalse(self.db.exists())

    def test_bad_preparation_options_do_not_touch_database(self):
        self.install()
        for options in [("--model",), ("--model", "missing"),
                        ("--resources", "invalid"), ("--name", "../escape"), ("--name", "-flag"),
                        ("--db", "/"), ("--unknown",)]:
            self.run_dev("setup", self.repo, "--db", self.db, *options, success=False)
        self.run_dev("refresh", self.repo, "--replace", success=False)
        self.assertFalse(self.db.exists())
        self.assertFalse(self.state.exists())


if __name__ == "__main__":
    unittest.main()
