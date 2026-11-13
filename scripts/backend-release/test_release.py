#!/usr/bin/env python3
"""Negative controls for package completeness, reproducibility and relocation."""
import json
import os
import tempfile
import unittest
from unittest.mock import patch
from common import Path, archive, digest, inventory, write_json
from verify import verify
from assemble import dependencies, source_notices


class SourceNoticeTests(unittest.TestCase):
    def test_real_algorithm_notices_are_collected_and_tampering_rejected(self):
        source = Path(__file__).resolve().parents[2]
        supplements = Path(__file__).resolve().parent / "license-supplements"
        with tempfile.TemporaryDirectory(prefix="pdrs-source-notices-") as temporary:
            root = Path(temporary)
            output = root / "output"
            source_notices(source, output)
            records = json.loads((output / "source-notices.json").read_text())["notices"]
            notice = next(n for n in records if n["id"] == "transcript-adapter-sort")
            self.assertEqual(len(notice["files"]), 3)
            for entry in notice["files"]:
                self.assertEqual(digest(output / entry["path"]), entry["sha256"])
            import shutil
            copied = root / "supplements"
            shutil.copytree(supplements, copied)
            (copied / "cpython-3.7.0-LICENSE.txt").write_text("incorrect replacement")
            with self.assertRaisesRegex(ValueError, "source notice changed"):
                source_notices(source, root / "tampered", copied)
            index = json.loads((copied / "index.json").read_text())
            index["sourceNotices"][0]["files"][0]["file"] = "../outside.txt"
            (root / "outside.txt").write_text("escaped")
            (copied / "index.json").write_text(json.dumps(index))
            with self.assertRaisesRegex(ValueError, "escapes supplement"):
                source_notices(source, root / "escaped", copied)


class PackageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="pdrs-release-tests-")
        self.root = Path(self.temp.name) / "package"
        (self.root / "bin").mkdir(parents=True)
        self.binary = self.root / "bin/pilotdeck"
        self.binary.write_bytes(b"synthetic packaging fixture, not a product binary\n")
        self.binary.chmod(0o755)
        self.reseal()

    def tearDown(self):
        self.temp.cleanup()

    def reseal(self):
        for name in ["manifest.json", "SHA256SUMS"]:
            (self.root / name).unlink(missing_ok=True)
        write_json(self.root / "manifest.json", {"schema": 1, "files": inventory(self.root)})
        (self.root / "SHA256SUMS").write_text("".join(f'{i["sha256"]}  {i["path"]}\n' for i in inventory(self.root) if i["type"] == "file"))

    def test_archive_is_identical_after_mtime_changes_and_relocation(self):
        a, b = Path(self.temp.name) / "a.tar.gz", Path(self.temp.name) / "b.tar.gz"
        archive(self.root, a, 1700000000, "release")
        os.utime(self.binary, (1800000000, 1800000000))
        moved = self.root.with_name("another-location")
        self.root.rename(moved)
        self.assertTrue(verify(moved)["verified"])
        archive(moved, b, 1700000000, "release")
        self.assertEqual(digest(a), digest(b))

    def test_changed_binary_is_rejected(self):
        self.binary.write_bytes(b"tampered")
        with self.assertRaisesRegex(ValueError, "inventory differs"):
            verify(self.root)

    def test_missing_and_extra_files_are_rejected(self):
        self.binary.unlink()
        with self.assertRaisesRegex(ValueError, "inventory differs"):
            verify(self.root)
        self.binary.write_bytes(b"replacement")
        self.reseal()
        (self.root / "extra").write_text("unexpected")
        with self.assertRaisesRegex(ValueError, "inventory differs"):
            verify(self.root)

    def test_executable_mode_is_verified(self):
        self.binary.chmod(0o644)
        with self.assertRaisesRegex(ValueError, "inventory differs"):
            verify(self.root)

    def test_external_and_broken_symlinks_are_rejected(self):
        link = self.root / "escape"
        link.symlink_to("../outside")
        with self.assertRaisesRegex(ValueError, "non-relocatable symlink"):
            inventory(self.root)
        link.unlink()
        link.symlink_to("bin/missing")
        with self.assertRaisesRegex(ValueError, "broken symlink"):
            inventory(self.root)

    def test_checksum_list_cannot_omit_a_payload(self):
        (self.root / "SHA256SUMS").write_text("")
        with self.assertRaisesRegex(ValueError, "checksum list"):
            verify(self.root)

    def dependency_fixture(self):
        source = Path(self.temp.name) / "source"
        (source / "src").mkdir(parents=True)
        (source / "LICENSE").write_text("fixture project license")
        (source / "src/Cargo.lock").write_text('version = 4\n[[package]]\nname = "codec"\nversion = "1.0.0"\nsource = "registry+fixture"\nchecksum = "fixture-checksum"\n')
        crate = Path(self.temp.name) / "codec"
        (crate / "native").mkdir(parents=True)
        (crate / "Cargo.toml").write_text('[package]\nname="codec"\nversion="1.0.0"\n')
        metadata = Path(self.temp.name) / "metadata.json"
        write_json(metadata, {"packages": [{"name": "codec", "version": "1.0.0", "source": "registry+fixture", "manifest_path": str(crate / "Cargo.toml"), "license": "MIT"}]})
        return source, crate, metadata

    def test_dependency_inventory_rejects_missing_notices_and_partial_metadata(self):
        source, crate, metadata = self.dependency_fixture()
        with self.assertRaisesRegex(ValueError, "no collected license"):
            dependencies(metadata, source, self.root)
        write_json(metadata, {"packages": []})
        with self.assertRaisesRegex(ValueError, "metadata is incomplete"):
            dependencies(metadata, source, self.root)

    def test_supplement_does_not_replace_embedded_native_notice(self):
        source, crate, metadata = self.dependency_fixture()
        (crate / "native/COPYING").write_text("native dependency license")
        wrapper = Path(self.temp.name) / "wrapper-license"
        wrapper.write_text("wrapper license")
        with patch("assemble.supplement", return_value=(wrapper, {"file": "wrapper-license"})):
            dependencies(metadata, source, self.root)
        result = json.loads((self.root / "dependencies.json").read_text())
        notices = result["packages"][0]["noticeFiles"]
        self.assertEqual(len(notices), 2)
        self.assertEqual({(self.root / p).read_text() for p in notices},
                         {"native dependency license", "wrapper license"})

    def test_native_dependency_notices_are_retained_with_wrapper_license(self):
        source, crate, metadata = self.dependency_fixture()
        (crate / "LICENSE").write_text("wrapper license")
        (crate / "native/COPYING").write_text("native dependency license")
        dependencies(metadata, source, self.root)
        collected = json.loads((self.root / "dependencies.json").read_text())["packages"][0]
        self.assertEqual(len(collected["noticeFiles"]), 2)
        self.assertEqual({(self.root / p).read_text() for p in collected["noticeFiles"]}, {"wrapper license", "native dependency license"})


if __name__ == "__main__":
    unittest.main()
