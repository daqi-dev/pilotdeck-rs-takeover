#!/usr/bin/env python3
"""`pilotdeck_minthub_scaffold.py` 纯函数单元测试（不触发真实 mint 下载）。

跑法：python3 -I scripts/lib/test_pilotdeck_minthub_scaffold.py
覆盖面：
- `[profile.dev.package.*]` 转发：只转发 dev.package 表、保留表体顺序与取值，
  不夹带 `[profile.dev]` / `[profile.release]` / 注释行；
- `render_workspace_cargo`：生成合法 TOML，workspace members/exclude/package/deps/lints
  照抄，kernel 依赖门控与 `agentcpm-iface` 覆盖路径行为不回退；
- `verify_mint_manifest_files`：dict/list 两种 manifest 形态的 SHA-256 校验与 fail-closed；
- `copy_pkg_crate_only`：`.git`/`.mint`/`target`/`dist`/`__pycache__` 不随包拷贝。
"""

import hashlib
import importlib.util
import json
import shutil
import sys
import tempfile
import tomllib
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location(
    "pilotdeck_minthub_scaffold", HERE / "pilotdeck_minthub_scaffold.py"
)
scaffold = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(scaffold)

ROOT_CARGO = """\
[workspace]
resolver = "3"
exclude = ["vendor/tiktoken-rs"]
members = ["pilotdeck-protocol"]

[workspace.package]
edition = "2024"
rust-version = "1.93"
version = "0.1.0"
license = "AGPL-3.0-only"
license-file = "../LICENSE"
publish = false

[workspace.dependencies]
agentcpm-iface = { path = "../deps/agent-kernel/kernel/iface" }
agentcpm-scheduler-protocol = { path = "../deps/agent-kernel/kernel/scheduler-protocol" }
serde_json = { version = "=1.0.151", features = ["raw_value"] }
sha2 = "=0.10.9"
pilotdeck-protocol = { path = "pilotdeck-protocol" }

[workspace.lints.rust]
unsafe_code = "forbid"

[workspace.lints.clippy]
all = "warn"
debug_assert_with_mut_call = "warn"

[patch.crates-io]
icu_collator = { path = "vendor/icu_collator" }

[profile.dev]
debug = 1

# Keep hashing and JSON serialization fast in dev/test builds.
[profile.dev.package.sha2]
opt-level = 3

[profile.dev.package.serde]
# inline comment must be skipped
opt-level = 3

[profile.dev.package.pilotdeck-tool-core]
opt-level = 2

[profile.release]
lto = "thin"
"""


def make_repo_root(base: Path) -> Path:
    repo = base / "repo"
    (repo / "src").mkdir(parents=True)
    (repo / "src" / "Cargo.toml").write_text(ROOT_CARGO, encoding="utf-8")
    return repo


class ExtractProfileDevPackageSections(unittest.TestCase):
    def test_forwards_only_dev_package_tables_in_order(self):
        lines = scaffold.extract_profile_dev_package_sections(ROOT_CARGO)
        self.assertEqual(
            lines,
            [
                "[profile.dev.package.sha2]",
                "opt-level = 3",
                "[profile.dev.package.serde]",
                "opt-level = 3",
                "[profile.dev.package.pilotdeck-tool-core]",
                "opt-level = 2",
            ],
        )

    def test_empty_when_no_dev_package_tables(self):
        self.assertEqual(
            scaffold.extract_profile_dev_package_sections("[profile.dev]\ndebug = 1\n"),
            [],
        )
        self.assertEqual(scaffold.extract_profile_dev_package_sections(""), [])

    def test_rejects_lookalike_headers(self):
        text = (
            "[profile.dev.packageX.sha2]\nopt-level = 3\n"
            "[profile.dev.package]\nopt-level = 3\n"
            "[profile.dev.package.]opt-level = 3\n"
            "[profile.dev.package .sha2]\nopt-level = 3\n"
        )
        self.assertEqual(scaffold.extract_profile_dev_package_sections(text), [])


class RenderWorkspaceCargo(unittest.TestCase):
    def test_forwards_profile_dev_package_and_drops_other_profiles(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo_root(Path(tmp))
            out = scaffold.render_workspace_cargo(
                repo,
                ["pilotdeck-protocol", "pilotdeck-tool-core"],
                ["vendor/tiktoken-rs"],
                include_kernel_deps=False,
            )
            doc = tomllib.loads(out)
            self.assertEqual(
                doc["workspace"]["members"],
                ["pilotdeck-protocol", "pilotdeck-tool-core"],
            )
            self.assertEqual(doc["workspace"]["exclude"], ["vendor/tiktoken-rs"])
            self.assertEqual(doc["workspace"]["package"]["version"], "0.1.0")
            self.assertEqual(
                doc["workspace"]["dependencies"]["serde_json"],
                {"version": "=1.0.151", "features": ["raw_value"]},
            )
            self.assertEqual(doc["workspace"]["lints"]["rust"], {"unsafe_code": "forbid"})
            self.assertEqual(
                doc["workspace"]["lints"]["clippy"],
                {"all": "warn", "debug_assert_with_mut_call": "warn"},
            )
            self.assertEqual(
                doc["patch"]["crates-io"],
                {"icu_collator": {"path": "vendor/icu_collator"}},
            )
            # 仅 profile.dev.package.* 被转发；[profile.dev] / [profile.release] 不夹带。
            self.assertEqual(set(doc["profile"].keys()), {"dev"})
            self.assertEqual(set(doc["profile"]["dev"].keys()), {"package"})
            self.assertEqual(
                doc["profile"]["dev"]["package"],
                {
                    "sha2": {"opt-level": 3},
                    "serde": {"opt-level": 3},
                    "pilotdeck-tool-core": {"opt-level": 2},
                },
            )
            # kernel 依赖在 include_kernel_deps=False 时被剔除。
            deps = doc["workspace"]["dependencies"]
            self.assertNotIn("agentcpm-iface", deps)
            self.assertNotIn("agentcpm-scheduler-protocol", deps)

    def test_kernel_deps_and_iface_override(self):
        with tempfile.TemporaryDirectory() as tmp:
            repo = make_repo_root(Path(tmp))
            full = scaffold.render_workspace_cargo(
                repo,
                ["pilotdeck-model-iface"],
                ["vendor/tiktoken-rs", "hub-agentcpm-iface"],
                include_kernel_deps=True,
            )
            doc = tomllib.loads(full)
            self.assertIn("agentcpm-iface", doc["workspace"]["dependencies"])
            self.assertIn("agentcpm-scheduler-protocol", doc["workspace"]["dependencies"])
            self.assertIn("profile", doc)

            probe = scaffold.render_workspace_cargo(
                repo,
                ["pilotdeck-model-iface"],
                ["vendor/tiktoken-rs", "hub-agentcpm-iface"],
                include_kernel_deps=False,
                iface_override_path="hub-agentcpm-iface",
            )
            probe_doc = tomllib.loads(probe)
            self.assertEqual(
                probe_doc["workspace"]["dependencies"]["agentcpm-iface"],
                {"path": "hub-agentcpm-iface"},
            )
            self.assertNotIn("agentcpm-scheduler-protocol", probe_doc["workspace"]["dependencies"])


class VerifyMintManifestFiles(unittest.TestCase):
    def _pkg(self, base: Path) -> Path:
        pkg = base / "pkg"
        pkg.mkdir()
        (pkg / "a.txt").write_bytes(b"alpha")
        (pkg / "nested").mkdir()
        (pkg / "nested" / "b.txt").write_bytes(b"beta")
        return pkg

    def test_dict_form_accepts_sha256_and_digest_prefixed(self):
        with tempfile.TemporaryDirectory() as tmp:
            pkg = self._pkg(Path(tmp))
            ha = hashlib.sha256(b"alpha").hexdigest()
            hb = "sha256:" + hashlib.sha256(b"beta").hexdigest()
            (pkg / "manifest.json").write_text(
                json.dumps({"files": {"a.txt": {"sha256": ha}, "nested/b.txt": hb}}),
                encoding="utf-8",
            )
            info = scaffold.verify_mint_manifest_files(pkg, "pkg")
            self.assertEqual(info, {"manifest_checked_files": 2})

    def test_list_form_and_mismatch_fail_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            pkg = self._pkg(Path(tmp))
            ha = hashlib.sha256(b"alpha").hexdigest()
            (pkg / "manifest.json").write_text(
                json.dumps({"files": [{"path": "a.txt", "sha256": ha}]}),
                encoding="utf-8",
            )
            self.assertEqual(
                scaffold.verify_mint_manifest_files(pkg, "pkg"),
                {"manifest_checked_files": 1},
            )
            (pkg / "a.txt").write_bytes(b"tampered")
            with self.assertRaises(SystemExit):
                scaffold.verify_mint_manifest_files(pkg, "pkg")

    def test_missing_manifest_or_file_fails_closed(self):
        with tempfile.TemporaryDirectory() as tmp:
            pkg = self._pkg(Path(tmp))
            with self.assertRaises(SystemExit):
                scaffold.verify_mint_manifest_files(pkg, "pkg")
            (pkg / "manifest.json").write_text(
                json.dumps({"files": {"gone.txt": {"sha256": "0" * 64}}}),
                encoding="utf-8",
            )
            with self.assertRaises(SystemExit):
                scaffold.verify_mint_manifest_files(pkg, "pkg")


class CopyPkgCrateOnly(unittest.TestCase):
    def test_build_artifacts_are_not_copied(self):
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            src = base / "src-pkg"
            for keep in ["Cargo.toml", "src/lib.rs"]:
                (src / Path(keep).parent).mkdir(parents=True, exist_ok=True)
                (src / keep).write_text("x", encoding="utf-8")
            for skip in [".git/config", ".mint/state", "target/debug/x", "dist/x", "__pycache__/x.pyc"]:
                p = src / skip
                p.parent.mkdir(parents=True, exist_ok=True)
                p.write_text("x", encoding="utf-8")
            dst = base / "dst-pkg"
            copied = scaffold.copy_pkg_crate_only(src, dst)
            self.assertEqual(copied, ["Cargo.toml", "src/lib.rs"])
            self.assertTrue((dst / "Cargo.toml").is_file())
            self.assertFalse((dst / "target").exists())
            self.assertFalse((dst / ".git").exists())
            # 再次调用时先清空旧目标，保证幂等重建。
            (dst / "stale.txt").write_text("x", encoding="utf-8")
            scaffold.copy_pkg_crate_only(src, dst)
            self.assertFalse((dst / "stale.txt").exists())


if __name__ == "__main__":
    unittest.main(verbosity=2)
