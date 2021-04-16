#!/usr/bin/env python3

import glob
import os
import os.path
import shutil
import subprocess
import sys

PLUGIN_DIRS = ['audio', 'generic', 'net', 'text', 'utils', 'video']

command, meson_build_dir, meson_current_source_dir, meson_build_root, target, exclude, extra_env, prefix, libdir = sys.argv[
    1:10]

cargo_target_dir = os.path.join(meson_build_dir, 'target')

env = os.environ.copy()
env['CARGO_TARGET_DIR'] = cargo_target_dir

pkg_config_path = env.get('PKG_CONFIG_PATH', '').split(':')
pkg_config_path.append(os.path.join(meson_build_root, 'meson-uninstalled'))
env['PKG_CONFIG_PATH'] = ':'.join(pkg_config_path)

if len(extra_env) > 0:
    for e in extra_env.split(','):
        k, v = e.split(':')
        env[k] = v

if command == 'build':
    # cargo build
    ext = sys.argv[10]
    # when using --default-library=both 2 extensions are passed
    try:
        ext2 = sys.argv[11]
    except IndexError:
        ext2 = None

    # Build with the 'static' feature enforcing the minimal gst version required for static builds
    cargo_cmd = ['cargo', 'cbuild', '--features', 'static']
    if target == 'release':
        cargo_cmd.append('--release')
elif command == 'test':
    # cargo test
    cargo_cmd = ['cargo', 'ctest', '--no-fail-fast', '--color=always']
else:
    print("Unknown command:", command)
    sys.exit(1)

cargo_cmd.extend(['--prefix', prefix, '--libdir',
                  os.path.join(prefix, libdir)])


def run(cargo_cmd, env):
    try:
        subprocess.run(cargo_cmd, env=env, check=True)
    except subprocess.SubprocessError:
        sys.exit(1)


for d in PLUGIN_DIRS:
    for name in os.listdir(os.path.join(meson_current_source_dir, d)):
        if '{}/{}'.format(d, name) in exclude:
            continue

        cargo_toml = os.path.join(
            meson_current_source_dir, d, name, 'Cargo.toml')
        cmd = cargo_cmd + ['--manifest-path', cargo_toml]
        run(cmd, env)

if command == 'build':
    target_dir = os.path.join(cargo_target_dir, '**', target)
    # Copy so files to build dir
    for f in glob.glob(os.path.join(target_dir, '*.' + ext), recursive=True):
        shutil.copy(f, meson_build_dir)
    if ext2:
        for f in glob.glob(os.path.join(target_dir, '*.' + ext2), recursive=True):
            shutil.copy(f, meson_build_dir)
    # Copy generated pkg-config files
    for f in glob.glob(os.path.join(target_dir, '*.pc'), recursive=True):
        shutil.copy(f, meson_build_dir)

    # Move -uninstalled.pc to meson-uninstalled
    uninstalled = os.path.join(meson_build_dir, 'meson-uninstalled')
    if not os.path.exists(uninstalled):
        os.mkdir(uninstalled)
    for f in glob.glob(os.path.join(meson_build_dir, '*-uninstalled.pc')):
        # move() does not allow us to update the file so remove it if it already exists
        dest = os.path.join(uninstalled, os.path.basename(f))
        if os.path.exists(dest):
            os.unlink(dest)
        shutil.move(f, uninstalled)
