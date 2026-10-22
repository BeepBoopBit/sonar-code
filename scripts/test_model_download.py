#!/usr/bin/env python3
"""Check shared model storage and download reuse without network access."""

import contextlib
import hashlib
import io
import json
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import check_embeddings
import download_model


@contextlib.contextmanager
def working_directory(path):
    previous = Path.cwd()
    try:
        os.chdir(path)
        yield
    finally:
        os.chdir(previous)


class ModelDownloadTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="sonar models ")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.home = self.root / "home with spaces"
        self.projects = [self.root / "first", self.root / "second"]
        for project in self.projects:
            project.mkdir()
        stack = contextlib.ExitStack()
        self.addCleanup(stack.close)
        stack.enter_context(mock.patch.object(Path, "home", return_value=self.home))
        self.assets = {name: name.encode() for name in download_model.MINILM_SHA256}
        for constant in ["SHA256", "MINILM_SHA256"]:
            checksums = {name: hashlib.sha256(self.assets[name]).hexdigest()
                         for name in getattr(download_model, constant)}
            stack.enter_context(mock.patch.object(download_model, constant, checksums))
        self.network = stack.enter_context(mock.patch.object(
            download_model.urllib.request, "urlopen", side_effect=self.response))

    def response(self, url, timeout):
        self.assertEqual(timeout, 60)
        name = url.split("/resolve/", 1)[1].split("/", 1)[1]
        return io.BytesIO(self.assets[name])

    def download(self, project, *args):
        output = io.StringIO()
        with working_directory(project), contextlib.redirect_stdout(output):
            download_model.main(list(args))
        return json.loads(output.getvalue())

    def test_models_are_shared_between_projects_and_valid_assets_are_not_rewritten(self):
        for model, directory, revision, hashes in [
            ("potion", "potion-code-16M-v2", download_model.REVISION, download_model.SHA256),
            ("minilm", "all-MiniLM-L6-v2", download_model.MINILM_REVISION,
             download_model.MINILM_SHA256),
        ]:
            with self.subTest(model=model):
                self.network.reset_mock()
                first = self.download(self.projects[0], "--model", model)
                shared = self.home / ".sonar/models" / directory
                self.assertEqual(self.network.call_count, len(hashes))
                self.assertEqual(first["revision"], revision)
                for name, digest in hashes.items():
                    self.assertEqual((shared / name).read_bytes(), self.assets[name])
                    self.assertEqual(first["files"][name]["sha256"], digest)
                    os.utime(shared / name, ns=(1_000_000_000, 1_000_000_000))
                before = {name: (shared / name).stat().st_mtime_ns for name in hashes}
                self.network.reset_mock()
                self.network.side_effect = AssertionError("unexpected download of cached model")
                second = self.download(self.projects[1], "--model", model)
                self.network.assert_not_called()
                self.assertEqual(second, first)
                self.assertEqual(before, {name: (shared / name).stat().st_mtime_ns
                                          for name in hashes})
                self.network.side_effect = self.response
        for project in self.projects:
            self.assertFalse((project / ".sonar").exists())

    def test_custom_destinations_do_not_require_a_home_directory(self):
        for destination in [self.root / "custom model", Path("relative model")]:
            with self.subTest(destination=destination):
                with mock.patch.object(Path, "home", side_effect=RuntimeError("no home")):
                    self.download(self.projects[0], "--destination", str(destination))
                actual = self.projects[0] / destination
                self.assertTrue((actual / "model.safetensors").is_file())
                self.assertFalse(self.home.exists())

    def test_only_missing_or_corrupt_assets_are_downloaded_again(self):
        self.download(self.projects[0])
        shared = self.home / ".sonar/models/potion-code-16M-v2"
        (shared / "config.json").unlink()
        (shared / "model.safetensors").write_bytes(b"damaged")
        self.network.reset_mock()
        self.download(self.projects[1])
        names = {call.args[0].rsplit("/", 1)[-1] for call in self.network.call_args_list}
        self.assertEqual(names, {"config.json", "model.safetensors"})
        self.assertEqual((shared / "model.safetensors").read_bytes(),
                         self.assets["model.safetensors"])

    def test_bad_download_preserves_existing_asset(self):
        self.download(self.projects[0])
        shared = self.home / ".sonar/models/potion-code-16M-v2"
        (shared / "config.json").write_bytes(b"old asset")
        self.network.side_effect = lambda *args, **kwargs: io.BytesIO(b"bad download")
        with self.assertRaisesRegex(SystemExit, "Checksum mismatch"):
            self.download(self.projects[1])
        self.assertEqual((shared / "config.json").read_bytes(), b"old asset")

    def test_embedding_checker_uses_shared_model_or_explicit_override(self):
        for project in self.projects:
            with working_directory(project):
                self.assertEqual(check_embeddings.parse_args([]).model,
                                 self.home / ".sonar/models/potion-code-16M-v2")
                with mock.patch.object(Path, "home", side_effect=RuntimeError("no home")):
                    self.assertEqual(check_embeddings.parse_args(["--model", "local model"]).model,
                                     Path("local model"))
        self.assertFalse(self.home.exists())


if __name__ == "__main__":
    unittest.main()
