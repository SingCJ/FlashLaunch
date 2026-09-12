"""Create a verified release archive containing the complete resource folders."""
import os
from pathlib import Path
import re
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[1]
RESOURCE_DIRECTORIES = ('Assets', 'Languages')
REQUIRED_FILES = ('Assets/Flash Launch.ico', 'Assets/fping.wav')


def checked_path(root, relative):
    path = root / relative
    for part in (path, *path.parents):
        if part == root:
            break
        if part.exists() and (part.is_symlink() or getattr(part.stat(), 'st_file_attributes', 0) & 0x400):
            raise ValueError(f'Reparse points are not allowed: {relative}')
    if not path.resolve().is_relative_to(root):
        raise ValueError(f'Path escapes the project: {relative}')
    return path


def release_files(root=ROOT):
    """Return every regular file shipped from Assets and Languages."""
    root = Path(root).resolve()
    files = ['Flash Launch.exe']
    for directory in RESOURCE_DIRECTORIES:
        resource_root = checked_path(root, directory)
        if not resource_root.is_dir():
            raise ValueError(f'Required release directory missing: {directory}')
        for path in resource_root.rglob('*'):
            relative = path.relative_to(root).as_posix()
            checked_path(root, relative)
            if path.is_symlink() or getattr(path.stat(), 'st_file_attributes', 0) & 0x400:
                raise ValueError(f'Reparse points are not allowed: {relative}')
            if path.is_file():
                files.append(relative)
    return tuple(files[:1] + sorted(files[1:]))


FILES = release_files(ROOT)
ASSETS = tuple(name.removeprefix('Assets/') for name in FILES if name.startswith('Assets/'))
LANGUAGES = tuple(
    name.removeprefix('Languages/').removesuffix('.ini')
    for name in FILES
    if name.startswith('Languages/') and name.endswith('.ini')
)


def package(root=ROOT, architecture='x64', executable=None, output_dir=None):
    root = Path(root).resolve()
    manifest = checked_path(root, 'Cargo.toml').read_text(encoding='utf-8-sig')
    match = re.search(r'(?m)^version\s*=\s*"(\d+\.\d+\.\d+)"\s*$', manifest)
    if not match:
        raise ValueError('Missing semantic package version')
    version = match[1]
    if architecture not in ('x64', 'x86'):
        raise ValueError('Unsupported architecture')
    executable_path = Path(executable).resolve() if executable else checked_path(root, 'Flash Launch.exe')
    if not executable_path.is_file():
        raise ValueError(f'Required release file missing: {executable_path.name}')
    files = release_files(root)
    for required in REQUIRED_FILES:
        if not checked_path(root, required).is_file():
            raise ValueError(f'Required release file missing: {required}')
    paths = [(name, executable_path if name == 'Flash Launch.exe' else checked_path(root, name)) for name in files]
    for name, path in paths:
        if not path.is_file():
            raise ValueError(f'Required release file missing: {name}')
    executable = paths[0][1].read_bytes()
    if executable[:2] != b'MZ' or len(executable) < 64:
        raise ValueError('Invalid Windows executable')
    pe = int.from_bytes(executable[60:64], 'little')
    if pe < 64 or pe + 6 > len(executable) or executable[pe:pe + 4] != b'PE\0\0':
        raise ValueError('Invalid PE header')
    machine = executable[pe + 4:pe + 6]
    expected = b'\x64\x86' if architecture == 'x64' else b'\x4c\x01'
    if machine != expected:
        raise ValueError(f'Expected a Windows {architecture} executable')

    name = f'FlashLaunch-{version}-windows-{architecture}.zip'
    destination_root = Path(output_dir).resolve() if output_dir else root
    destination = destination_root / name
    destination.parent.mkdir(parents=True, exist_ok=True)
    staging = checked_path(root, 'AI_CLI_TEMP')
    created_staging = not staging.exists()
    staging.mkdir(exist_ok=True)
    try:
        with tempfile.TemporaryDirectory(prefix='release-', dir=staging) as directory:
            temporary = Path(directory) / name
            with zipfile.ZipFile(temporary, 'w', zipfile.ZIP_DEFLATED, compresslevel=9) as archive:
                for relative, path in paths:
                    archive.write(path, relative)
            with zipfile.ZipFile(temporary) as archive:
                if set(archive.namelist()) != set(files) or archive.testzip() is not None:
                    raise ValueError('Archive verification failed')
                for relative, path in paths:
                    if archive.read(relative) != path.read_bytes():
                        raise ValueError(f'Archive content mismatch: {relative}')
            os.replace(temporary, destination)
    finally:
        if created_staging:
            staging.rmdir()
    print(f'Verified {len(files)} files: {destination.name}')
    return destination


if __name__ == '__main__':
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument('--architecture', choices=('x64', 'x86'), default='x64')
    parser.add_argument('--executable')
    parser.add_argument('--output-dir')
    args = parser.parse_args()
    package(architecture=args.architecture, executable=args.executable, output_dir=args.output_dir)
