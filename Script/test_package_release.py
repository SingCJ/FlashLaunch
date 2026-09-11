"""Exercise packaging in isolated project-local fixtures without starting the app."""
from pathlib import Path
import shutil
import tempfile
import unittest
import zipfile

from package_release import ROOT, FILES, checked_path, package


class ReleasePackagingTests(unittest.TestCase):
    def setUp(self):
        staging = ROOT / 'TEMP'
        staging.mkdir(exist_ok=True)
        self.temporary = tempfile.TemporaryDirectory(prefix='package-test-', dir=staging)
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        (self.root / 'Cargo.toml').write_text('[package]\nversion = "7.8.9"\n', encoding='utf-8')
        for name in FILES:
            destination = self.root / name
            destination.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(ROOT / name, destination)

    def test_package_is_allowlisted_and_hashes_match(self):
        (self.root / 'CONFIG').mkdir()
        (self.root / 'CONFIG' / 'private.ini').write_text('personal data')
        (self.root / 'Assets' / 'private.txt').write_text('must not ship')
        archive_path = package(self.root)
        self.assertEqual(archive_path.name, 'FlashLaunch-7.8.9-windows-x64.zip')
        with zipfile.ZipFile(archive_path) as archive:
            self.assertEqual(set(archive.namelist()), set(FILES))
            self.assertIn('Assets/fping.wav', archive.namelist())
            self.assertIn('Assets/Flash Launch.ico', archive.namelist())
        self.assertFalse((self.root / 'AI_CLI_TEMP').exists())

    def test_missing_sound_does_not_replace_previous_package(self):
        previous = self.root / 'FlashLaunch-7.8.9-windows-x64.zip'
        previous.write_bytes(b'previous release')
        (self.root / 'Assets' / 'fping.wav').unlink()
        with self.assertRaisesRegex(ValueError, 'Required release file missing'):
            package(self.root)
        self.assertEqual(previous.read_bytes(), b'previous release')

    def test_wrong_architecture_is_rejected(self):
        (self.root / 'Flash Launch.exe').write_bytes(b'not an executable')
        with self.assertRaisesRegex(ValueError, 'Invalid Windows executable'):
            package(self.root)

    def test_package_accepts_x86_machine(self):
        source = bytearray((self.root / 'Flash Launch.exe').read_bytes())
        pe = int.from_bytes(source[60:64], 'little')
        source[pe + 4:pe + 6] = b'\x4c\x01'
        executable = self.root / 'Flash Launch x86.exe'
        executable.write_bytes(source)

        archive_path = package(self.root, architecture='x86', executable=executable)

        self.assertEqual(archive_path.name, 'FlashLaunch-7.8.9-windows-x86.zip')
        with zipfile.ZipFile(archive_path) as archive:
            self.assertEqual(set(archive.namelist()), set(FILES))
            self.assertEqual(archive.read('Flash Launch.exe'), source)

    def test_escape_is_rejected(self):
        with self.assertRaisesRegex(ValueError, 'escapes the project'):
            checked_path(self.root, '../outside.txt')


if __name__ == '__main__':
    unittest.main()
