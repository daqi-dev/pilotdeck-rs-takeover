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


class RipgrepPackagingTests(unittest.TestCase):
    """Controlled packaging fake only; never evidence of actual ripgrep behavior."""
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='pdrs-ripgrep-structure-')
        self.root = Path(self.temp.name)
        self.prefix = self.root / 'prefix'
        for directory in ['bin','share/source','share/licenses','share/evidence']:
            (self.prefix / directory).mkdir(parents=True)
        self.target = 'aarch64-unknown-linux-gnu'
        self.binary = self.prefix / 'bin/rg'
        self.binary.write_bytes(b'CONTROLLED FAKE: packaging structure only, not an engine')
        self.binary.chmod(0o755)
        (self.prefix / 'share/source/source.txt').write_text('fake source fixture')
        (self.prefix / 'share/licenses/LICENSE').write_text('fake license fixture')
        from ripgrep_component import SOURCE_PINS, LICENSE_PINS, distribution_identity
        (self.prefix/'share/source/Cargo.lock').write_text('controlled fake lock')
        vendor=self.prefix/'share/source/vendor';vendor.mkdir()
        (vendor/'.cargo-checksum.json').write_text('{}')
        lock_sha=digest(self.prefix/'share/source/Cargo.lock')
        self.pins_patch=patch.dict(SOURCE_PINS,{'Cargo.lock.sha256':lock_sha})
        self.pins_patch.start()
        license_sha=digest(self.prefix/'share/licenses/LICENSE')
        self.licenses_patch=patch.dict(LICENSE_PINS,{role:{license_sha} for role in ['ripgrep','pcre2','sljit']})
        self.licenses_patch.start()
        texts={
          'rg-command':'cargo build --locked --release --features pcre2',
          'rg-exit':'0',
          'pcre-command':'cmake -DBUILD_SHARED_LIBS=OFF -DPCRE2_SUPPORT_JIT=ON',
          'pcre-exit':'{"exit":0}',
          'platform':'controlled fake aarch64 Linux',
          'version':'ripgrep 15.0.0\nPCRE2 10.45 is available (JIT is available)\n',
          'recipe':'PCRE2_SYS_STATIC=0 PKG_CONFIG_ALL_STATIC=1',
        }
        evidence=[]
        for role,text in texts.items():
            file=self.prefix/f'share/evidence/{role}.txt';file.write_text(text)
            evidence.append({'role':role,'path':file.relative_to(self.prefix).as_posix(),'sha256':digest(file)})
        entry=lambda relative: distribution_identity(self.prefix,relative)
        self.record={'schema':1,'name':'ripgrep','version':'15.0.0','target':self.target,'sourceBuilt':True,
                     'pcre2':{'version':'10.45','jit':True},'binary':{'path':'bin/rg','sha256':digest(self.binary)},
                     'sourcePins':dict(SOURCE_PINS),'distribution':{
                       'sources':{role:entry('share/source/source.txt') for role in ['ripgrep','pcre2','sljit']},
                       'licenses':{role:[entry('share/licenses/LICENSE')] for role in ['ripgrep','pcre2','sljit']},
                       'cargoLock':entry('share/source/Cargo.lock'),'vendor':entry('share/source/vendor')},
                     'buildEvidence':evidence,'files':inventory(self.prefix)}
        self.write_record()

    def tearDown(self):
        self.licenses_patch.stop()
        self.pins_patch.stop()
        self.temp.cleanup()

    def write_record(self):
        write_json(self.prefix/'share/ripgrep-build.json',self.record)

    def test_copy_and_moved_verify_close_over_sources_licenses_and_record(self):
        from ripgrep_component import copy_ripgrep
        package=self.root/'package';package.mkdir()
        with patch('ripgrep_component.binary_target',return_value=self.target):
            component=copy_ripgrep(self.prefix,package,self.target)
            write_json(package/'manifest.json',{'schema':1,'variant':'core','target':self.target,'components':[component],
                                               'requiredComponents':['ripgrep'],'files':inventory(package)})
            (package/'SHA256SUMS').write_text(''.join(f'{item["sha256"]}  {item["path"]}\n' for item in inventory(package) if item['type']=='file'))
            self.assertTrue(verify(package)['verified'])
            (package/'components/ripgrep/share/licenses/LICENSE').write_text('changed')
            with self.assertRaisesRegex(ValueError,'inventory differs'):
                verify(package)

    def test_core_without_mandatory_engine_is_rejected(self):
        package=self.root/'package';package.mkdir()
        write_json(package/'manifest.json',{'schema':1,'variant':'core','files':[]})
        (package/'SHA256SUMS').write_text('')
        with self.assertRaisesRegex(ValueError,'requires source-built ripgrep'):
            verify(package)

    def test_wrong_version_or_jit_is_rejected(self):
        from ripgrep_component import validate_ripgrep
        for version,jit in [('15.1.0',True),('15.0.0',False)]:
            self.record['version']=version;self.record['pcre2']['jit']=jit;self.write_record()
            with self.assertRaisesRegex(ValueError,'version/PCRE2 JIT'):
                validate_ripgrep(self.prefix,self.target)

    def test_missing_provenance_and_wrong_arch_are_rejected(self):
        from ripgrep_component import validate_ripgrep
        with patch('ripgrep_component.binary_target',return_value='x86_64-unknown-linux-gnu'):
            with self.assertRaisesRegex(ValueError,'architecture differs'):
                validate_ripgrep(self.prefix,self.target)
        self.record['buildEvidence']=[];self.write_record()
        with patch('ripgrep_component.binary_target',return_value=self.target):
            with self.assertRaisesRegex(ValueError,'required evidence roles'):
                validate_ripgrep(self.prefix,self.target)

    def test_duplicate_evidence_is_rejected(self):
        from ripgrep_component import validate_ripgrep
        self.record['buildEvidence']=[self.record['buildEvidence'][0]]*7;self.write_record()
        with patch('ripgrep_component.binary_target',return_value=self.target):
            with self.assertRaisesRegex(ValueError,'distinct paths'):
                validate_ripgrep(self.prefix,self.target)

    def test_real_nonzero_exit_is_rejected_even_with_matching_hash(self):
        from ripgrep_component import validate_ripgrep
        entry=next(x for x in self.record['buildEvidence'] if x['role']=='rg-exit')
        (self.prefix/entry['path']).write_text('1');entry['sha256']=digest(self.prefix/entry['path'])
        self.record['files']=[x for x in inventory(self.prefix) if x['path']!='share/ripgrep-build.json'];self.write_record()
        with patch('ripgrep_component.binary_target',return_value=self.target):
            with self.assertRaisesRegex(ValueError,'exit must be actual zero'):
                validate_ripgrep(self.prefix,self.target)

    def test_missing_vendor_role_is_rejected(self):
        from ripgrep_component import validate_ripgrep
        del self.record['distribution']['vendor'];self.write_record()
        with patch('ripgrep_component.binary_target',return_value=self.target):
            with self.assertRaisesRegex(ValueError,'Cargo.lock and vendor'):
                validate_ripgrep(self.prefix,self.target)

    def test_smoke_search_requires_both_successful_tool_receipts(self):
        from smoke import search_receipts
        messages=[];frames=[]
        for tool,marker in [('grep','RELEASE_NATIVE_GREP_OK'),('glob','release-search-probe.txt')]:
            tool_id='release-'+tool
            messages.append({'role':'tool','tool_call_id':tool_id,'content':marker})
            frames.append({'event':{'type':'tool_call_finished','toolCallId':tool_id,'ok':True,'resultPreview':marker}})
        records=[{'body':{'messages':messages}}]
        self.assertEqual(set(search_receipts(records,frames)),{'grep','glob'})
        frames[-1]['event']['ok']=False
        with self.assertRaisesRegex(AssertionError,'did not execute successfully'):
            search_receipts(records,frames)


if __name__ == "__main__":
    unittest.main()
