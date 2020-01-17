#!/usr/bin/env python3

import glob
import os
import os.path
import shutil
import subprocess
import sys

command, meson_build_dir, meson_current_source_dir, meson_build_root, target, exclude, extra_env = sys.argv[
    1:8]

cargo_target_dir = os.path.join(meson_build_dir, 'target')

env = os.environ.copy()
env['CARGO_TARGET_DIR'] = cargo_target_dir

# FIXME: hack so cargo will find gst libs when building inside gst-build.
# We should fetch this from meson deps instead of hardcoding the paths,
# when Meson will generate -uninstalled.pc files, they all will be in
# <meson_build_root>/meson-uninstalled/
pkg_config_path = env.get('PKG_CONFIG_PATH', '').split(':')
pkg_config_path.append(os.path.join(
    meson_build_root, 'subprojects', 'gstreamer', 'pkgconfig'))
pkg_config_path.append(os.path.join(
    meson_build_root, 'subprojects', 'gst-plugins-base', 'pkgconfig'))
env['PKG_CONFIG_PATH'] = ':'.join(pkg_config_path)

if len(extra_env) > 0:
    for e in extra_env.split(','):
        k, v = e.split(':')
        env[k] = v

if command == 'build':
    # cargo build
    ext = sys.argv[8]
    cargo_cmd = ['cargo', 'build', '--all-targets',
                 '--manifest-path', os.path.join(
                     meson_current_source_dir, 'Cargo.toml'),
                 '--workspace']
    if target == 'release':
        cargo_cmd.append('--release')
elif command == 'test':
    # cargo test
    cargo_cmd = ['cargo', 'test', '--no-fail-fast', '--color=always', '--manifest-path',
                 os.path.join(meson_current_source_dir, 'Cargo.toml'),
                 '--workspace']
else:
    print("Unknown command:", command)
    sys.exit(1)

if len(exclude) > 0:
    for e in exclude.split(','):
        cargo_cmd.append('--exclude')
        cargo_cmd.append(e)

try:
    subprocess.run(cargo_cmd, env=env, check=True)
except subprocess.SubprocessError:
    sys.exit(1)

if command == 'build':
    # Copy so files to build dir
    for f in glob.glob(os.path.join(cargo_target_dir, target, '*.' + ext)):
        shutil.copy(f, meson_build_dir)
