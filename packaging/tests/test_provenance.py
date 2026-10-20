import sys
from pathlib import Path
ROOT = Path(__file__).resolve().parents[2]
sys.path[:0] = [str(ROOT / "build-support"), str(ROOT / "build-support/update")]
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
import hashlib
import os

ROOT = Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location('provenance', Path(__file__).parents[2] / 'build-support/provenance.py')
P = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(P)


class Provenance(unittest.TestCase):
    def test_runtime_file_macros_cannot_leak_builder_paths(self):
        with tempfile.TemporaryDirectory() as temp:
            image = Path(temp) / 'image'
            private = '/build/private/source'
            for path in (private + '/codec.c', private.upper() + '/codec.c',
                         private.replace('/', '\\') + '\\codec.c'):
                for encoding in ('utf-8', 'utf-16le'):
                    image.write_bytes(b'header' + path.encode(encoding) + b'\0')
                    with self.assertRaisesRegex(RuntimeError, 'Builder filesystem path') as caught:
                        P.verify_no_builder_paths(image, [private])
                    self.assertNotIn(private, str(caught.exception))
            image.write_bytes(b'cargo/registry/codec.c\0oink/src/main.rs\0')
            P.verify_no_builder_paths(image, [private])

    def test_export_boundary_keeps_product_inputs_and_only_approved_documents(self):
        for path in ('Cargo.lock', 'build-support/build.py', 'apps/oink/src/main.ts',
                     'README.md', 'README.zh-CN.md', 'README.ja.md', 'README.ko.md',
                     'LICENSE-ADDITIONAL-TERMS.md', 'PRIVACY.md', 'THIRD_PARTY_LICENSES.md', 'licenses/native/LICENSE.md'):
            self.assertTrue(P.product_source(path), path)
        for path in ('.claude/agents/developer.md', 'AGENTS.md', 'CLAUDE.md',
                     'RELEASING.md', 'docs/release/packaging.md', 'docs/nested/example.md', 'docs/example.md', 'build-support/update/README.md',
                     'crates/example/README.md', 'scripts/dev.sh', 'tools/probe/main.swift',
                     'target/output', 'evidence/check.json'):
            self.assertFalse(P.product_source(path), path)

    def test_archive_hash_survives_tauri_generated_ide_schemas(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            source = root / 'apps/oink/src-tauri'
            source.mkdir(parents=True)
            (source / 'Cargo.toml').write_text('[package]\nname = "fixture"\n')
            inventory = P.source_inventory(root)
            before = P.source_tree_sha256(root, inventory)
            generated = source / 'gen/schemas'
            generated.mkdir(parents=True)
            (generated / 'capabilities.json').write_text('{"generated":true}')
            self.assertEqual(P.source_tree_sha256(root, inventory), before)
            (source / 'new-source.rs').write_text('fn main() {}')
            with self.assertRaisesRegex(RuntimeError, 'inventory changed'):
                P.source_tree_sha256(root, inventory)

    def test_tree_hash_is_sorted_paths_modes_and_bytes(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'z').write_bytes(b'last\0bytes')
            (root / 'a').write_bytes(b'first')
            for directory in ('build', 'target', '.git', 'evidence', 'node_modules'):
                (root / directory).mkdir()
                (root / directory / 'ignored').write_bytes(b'not source')
            # A .git file/directory only selects Git inventory for actual repos.
            (root / '.git').rename(root / 'build/git-metadata')
            expected = hashlib.sha256(b'a\x00100644\x005\x00firstz\x00100644\x0010\x00last\x00bytes').hexdigest()
            self.assertEqual(P.source_tree_sha256(root), expected)
            inventory = P.source_inventory(root)
            self.assertEqual(P.source_tree_sha256(root, list(reversed(inventory))), expected)
            (root / 'z').write_bytes(b'changed')
            self.assertNotEqual(P.source_tree_sha256(root), expected)
            with self.assertRaisesRegex(RuntimeError, 'Source changed'):
                P.source_tree_sha256(root, inventory)

    @unittest.skipIf(os.name == 'nt', 'POSIX mode and symlink semantics')
    def test_executable_modes_and_symlink_text_are_source(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'script').write_bytes(b'code')
            initial = P.source_tree_sha256(root)
            (root / 'script').chmod(0o755)
            self.assertNotEqual(initial, P.source_tree_sha256(root))
            (root / 'link').symlink_to('script')
            inventory = P.source_inventory(root)
            link = next(entry for entry in inventory if entry['path'] == 'link')
            self.assertEqual(link['mode'], '120000')
            self.assertEqual(link['sha256'], hashlib.sha256(b'script').hexdigest())

    def test_stale_and_tampered_binaries_are_rejected(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            binary = directory / 'app'
            binary.write_bytes(b'test build bytes')
            record = dict(source_tree_sha256='a' * 64, version='1.0.0', build=16, target='target',
                          identity_sha256=P.digest(P.ROOT / 'packaging/identity.json'), files={'app': P.digest(binary)})
            (directory / 'release-build.json').write_text(json.dumps(record))
            P.verify(directory, 'a' * 64, '1.0.0', 'target', 16)
            with self.assertRaises(RuntimeError):
                P.verify(directory, 'b' * 64, '1.0.0', 'target', 16)
            binary.write_bytes(b'changed after build')
            with self.assertRaises(RuntimeError):
                P.verify(directory, 'a' * 64, '1.0.0', 'target', 16)

    def test_build_must_be_present_numeric_and_match(self):
        with tempfile.TemporaryDirectory() as temp:
            directory = Path(temp)
            record = dict(source_tree_sha256='a' * 64, version='1.0.0', target='target',
                          identity_sha256=P.digest(P.ROOT / 'packaging/identity.json'), files={})
            for build in [None, 0, 15, 17, '16', 16.0, True]:
                with self.subTest(record_build=build):
                    if build is not None:
                        record['build'] = build
                    (directory / 'release-build.json').write_text(json.dumps(record))
                    with self.assertRaisesRegex(RuntimeError, 'Binary release build'):
                        P.verify(directory, 'a' * 64, '1.0.0', 'target', 16)
            record['build'] = 16
            (directory / 'release-build.json').write_text(json.dumps(record))
            self.assertEqual(P.verify(directory, 'a' * 64, '1.0.0', 'target', 16)['build'], 16)

    def test_signpath_configuration_tracks_identity(self):
        spec = importlib.util.spec_from_file_location('identity', Path(__file__).parents[2] / 'build-support/identity.py')
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        self.assertEqual((P.ROOT / '.signpath/artifact-configurations/windows-msi-v1.xml').read_bytes(), module.signpath_configuration())


if __name__ == '__main__':
    unittest.main()
