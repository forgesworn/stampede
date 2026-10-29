#!/usr/bin/env python3
"""Build and pack the existing guest with public dependencies and remapped paths."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
COMPILER = 'rustc 1.99.1 (58cf8f9c3 2026-10-13)'


def main():
    if len(sys.argv) != 2:
        raise SystemExit('usage: build-component.py COMPONENT_OUTPUT.wasm')
    output = Path(sys.argv[1]).resolve()
    if output == ROOT or ROOT in output.parents:
        raise ValueError('component output belongs outside the source checkout')
    compiler = subprocess.check_output(['rustup', 'run', '1.99.1', 'rustc', '--version'], text=True).strip()
    if compiler != COMPILER:
        raise ValueError('compiler identity mismatch')
    target = Path(os.environ['CARGO_TARGET_DIR']).resolve()
    fray = ROOT.parent / 'forgesworn/fray'
    mappings = [
        (Path.home(), '/build-home'),
        (Path(os.environ.get('CARGO_HOME', Path.home() / '.cargo')).resolve(), '/cargo'),
        (Path(os.environ.get('RUSTUP_HOME', Path.home() / '.rustup')).resolve(), '/rustup'),
        (ROOT.parent, '/sources'),
        (ROOT, '/sources/Stampede'),
        (fray, '/sources/forgesworn/fray'),
        (fray.resolve(), '/sources/forgesworn/fray'),
    ]
    flags = ['--remap-path-prefix=' + str(source) + '=' + destination for source, destination in mappings]
    env = dict(os.environ)
    for name in ['RUSTFLAGS', 'CARGO_ENCODED_RUSTFLAGS', 'CARGO_TARGET_WASM32_UNKNOWN_UNKNOWN_RUSTFLAGS']:
        if env.get(name):
            raise ValueError('qualification build requires its recorded compiler flags')
    env['CARGO_ENCODED_RUSTFLAGS'] = '\x1f'.join(flags)
    subprocess.run(['cargo', '+1.99.1', 'build', '--locked', '--release', '--target', 'wasm32-unknown-unknown',
        '--manifest-path', str(ROOT / 'qualification/guest/Cargo.toml')], cwd=ROOT, env=env, check=True)
    core = target / 'wasm32-unknown-unknown/release/race_authority.wasm'
    output.parent.mkdir(parents=True, exist_ok=True)
    subprocess.run(['cargo', '+1.99.1', 'run', '--locked', '--manifest-path',
        str(ROOT / 'qualification/component-pack/Cargo.toml'), '--', str(core), str(output)], cwd=ROOT, check=True)
    component = output.read_bytes()
    for path, _ in mappings:
        if len(str(path)) > 1 and str(path).encode() in component:
            raise ValueError('local build path remains in the packed component')
    report = dict(compiler=compiler, core_sha256=hashlib.sha256(core.read_bytes()).hexdigest(),
        component_sha256=hashlib.sha256(component).hexdigest(), component_bytes=len(component),
        source_paths_remapped=True, qualification_claimed=False)
    print(json.dumps(report))


if __name__ == '__main__':
    main()
