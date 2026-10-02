"""Build reviewable local distribution archives from an explicit file allowlist.

Run from any directory: python reference/package.py --output reference/dist
Never includes commercial files, case evidence, Git metadata or arbitrary globs.
"""
from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path
import zipfile

ROOT = Path(__file__).resolve().parent
SOURCE_FILES = (
    '.gitignore', 'LICENSE', 'README.md', 'PROTOCOL.md', 'ACCEPTANCE.md',
    'THIRD_PARTY_NOTICES.md', 'bootstrap.ps1', 'build.ps1', 'client.py', 'app_bridge.py', 'sender.py', 'package.py',
    'native/agent.cpp', 'native/common.hpp', 'native/delivery.hpp',
    'native/provider.hpp', 'native/describe.cpp',
    'tests/delivery_tests.cpp', 'tests/identification_tests.cpp', 'tests/fixture.cpp', 'tests/test_client.py', 'tests/test_app_bridge.py',
    'tests/test_package.py', 'tests/test_sender.py', 'tests/test_app_bridge_runtime.py', 'tests/test_runtime.py',
    'examples/control_parameters.py', 'profiles/autotune-pro-38c42d0b-x64.json',
)
LICENSE_FILES = {
    'third_party/pluginterfaces-LICENSE.txt': 'vendor/pluginterfaces/LICENSE.txt',
    'third_party/minhook-LICENSE.txt': 'vendor/minhook/LICENSE.txt',
}
BINARY_FILES = ('build/x64/reference_agent.dll', 'build/x64/describe.exe')


def digest(data):
    return hashlib.sha256(data).hexdigest()


def collect(binary):
    mapping = {name: name for name in SOURCE_FILES}
    mapping.update(LICENSE_FILES)
    if binary:
        mapping.update({name: name for name in BINARY_FILES})
    payload = {}
    for destination, source in mapping.items():
        path = ROOT / source
        resolved = path.resolve(strict=True)
        if not resolved.is_relative_to(ROOT.resolve()):
            raise ValueError(f'Input escapes the reference directory: {source}')
        payload['reference/' + destination] = resolved.read_bytes()
    return payload


def write_archive(path, payload):
    manifest = {
        'schema_version': 1,
        'files': [{'path': name, 'bytes': len(data), 'sha256': digest(data)}
                  for name, data in sorted(payload.items())],
    }
    manifest_bytes = (json.dumps(manifest, indent=2, ensure_ascii=False) + '\n').encode('utf-8')
    with zipfile.ZipFile(path, mode='x', compression=zipfile.ZIP_DEFLATED) as archive:
        for name, data in sorted(payload.items()):
            archive.writestr(name, data)
        archive.writestr('reference/MANIFEST.json', manifest_bytes)
    # Check actual archive members and bytes, including license texts.
    with zipfile.ZipFile(path) as archive:
        if archive.testzip() is not None:
            raise ValueError(f'Corrupt archive: {path}')
        if set(archive.namelist()) != set(payload) | {'reference/MANIFEST.json'}:
            raise ValueError('Archive contains unexpected entries')
        for name, data in payload.items():
            if archive.read(name) != data:
                raise ValueError(f'Archive verification mismatch: {name}')
    return {'filename': path.name, 'bytes': path.stat().st_size,
            'sha256': digest(path.read_bytes()), 'member_count': len(payload) + 1}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--output', type=Path, default=ROOT / 'dist')
    args = parser.parse_args()
    output = args.output.resolve()
    names = ('autotune-reference-source.zip', 'autotune-reference-windows-x64.zip', 'SHA256SUMS.json')
    if any((output / name).exists() for name in names):
        parser.error('Distribution files already exist; choose a new output directory')
    source = collect(False)
    binary = collect(True)
    output.mkdir(parents=True, exist_ok=True)
    packages = [write_archive(output / names[0], source), write_archive(output / names[1], binary)]
    result = {'packages': packages, 'commercial_files_included': False,
              'evidence_included': False, 'publication_performed': False}
    with (output / names[2]).open('x', encoding='utf-8') as handle:
        json.dump(result, handle, indent=2, ensure_ascii=False)
    print(json.dumps(result, indent=2))


if __name__ == '__main__':
    main()
