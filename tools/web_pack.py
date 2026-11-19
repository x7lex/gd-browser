#!/usr/bin/env python3
"""Build the browser version into dist/ (see web/ for the app itself).

Runs the wasm build (wasm-pack), the manifest generator (web_index) and
copies everything the page serves:

  dist/
    index.html app.js worker.js     the app (web/)
    pkg/gdrust.js + gdrust_bg.wasm  the engine, compiled to wasm
    data/objects.json               the object table
    levels.json                     what to play (from web_index)
    assets.json                     which game-art files exist + the wanted list
    assets/...                      game art, when a GD install is known
    levels/ audio/ macros/          levels, songs, macros

usage: python3 tools/web_pack.py [--serve] ; --serve also starts a local
server on http://localhost:8000 afterwards.
"""
import argparse
import json
import os
import shutil
import subprocess
import sys

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
DIST = os.path.join(ROOT, 'dist')


def run(cmd, **kw):
    print('$', ' '.join(cmd))
    subprocess.run(cmd, check=True, **kw)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--serve', action='store_true', help='serve dist/ on :8000 after packing')
    ap.add_argument('--skip-build', action='store_true', help='reuse web/pkg and the web_index output')
    args = ap.parse_args()

    if not args.skip_build:
        # the engine: wasm-pack builds only the lib (--lib), so the native
        # bins (minifb & co, no wasm) never enter this build
        run(['wasm-pack', 'build', '--release', '--target', 'web',
             '--out-name', 'gdrust', '--out-dir', 'web/pkg'], cwd=ROOT)
        run(['cargo', 'build', '--release', '--bin', 'web_index'], cwd=ROOT)

    if os.path.isdir(DIST):
        shutil.rmtree(DIST)
    os.makedirs(DIST)

    # the app + the engine
    for f in ('index.html', 'app.js', 'worker.js'):
        shutil.copy(os.path.join(ROOT, 'web', f), DIST)
    shutil.copytree(os.path.join(ROOT, 'web', 'pkg'), os.path.join(DIST, 'pkg'))

    # the object table
    os.makedirs(os.path.join(DIST, 'data'))
    shutil.copy(os.path.join(ROOT, 'data', 'objects.json'), os.path.join(DIST, 'data'))

    # levels.json + assets.json + everything they point at
    manifest = json.loads(subprocess.run([os.path.join(ROOT, 'target/release/web_index')],
                                         capture_output=True, check=True, cwd=ROOT).stdout)

    copied = set()

    def copy(repo_rel, dest_rel=None):
        dest_rel = dest_rel or repo_rel
        src, dst = os.path.join(ROOT, repo_rel), os.path.join(DIST, dest_rel)
        if repo_rel in copied or not os.path.exists(src):
            return
        copied.add(repo_rel)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        shutil.copy(src, dst)

    for level in manifest['levels']:
        copy(level['gmd'])
        if not copy(level['meta']):
            # deleted levels (the fetch failed): write the synthesized meta
            m = os.path.join(DIST, level['meta'])
            os.makedirs(os.path.dirname(m), exist_ok=True)
            json.dump({'name': level['name'], 'song': None}, open(m, 'w'))
        if level.get('macro'):
            copy(level['macro']['file'])
        if level.get('song'):
            copy(level['song'])

    for asset in manifest['assets']:
        src = os.path.join(ROOT, asset['path'])
        dst = os.path.join(DIST, asset['file'])
        copied.add(asset['file'])
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        shutil.copy(src, dst)

    json.dump({'levels': manifest['levels'],
               'assets': manifest['assets'],
               'wanted': manifest['wanted']},
              open(os.path.join(DIST, 'levels.json'), 'w'))
    json.dump({'files': manifest['assets'], 'wanted': manifest['wanted']},
              open(os.path.join(DIST, 'assets.json'), 'w'))

    mb = lambda n: f'{n / 1e6:.1f} MB'
    total = sum(os.path.getsize(os.path.join(dp, f)) for dp, _, fs in os.walk(DIST) for f in fs)
    print(f'packed dist/: {len(manifest["levels"])} levels, {len(manifest["assets"])} art files, '
          f'{len(copied)} files, {mb(total)} total')

    if args.serve:
        os.chdir(DIST)
        print('serving http://localhost:8000 (Ctrl-C to stop)')
        subprocess.run([sys.executable, '-m', 'http.server', '8000'])


if __name__ == '__main__':
    main()
