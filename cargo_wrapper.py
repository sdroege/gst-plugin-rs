#!/usr/bin/env python3

import glob
import os
import os.path
import shutil
import subprocess
import sys

meson_build_dir, meson_current_source_dir, meson_build_root, target, ext, exclude = sys.argv[1:]

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

# cargo build
cargo_cmd = ['cargo', 'build', '--manifest-path',
             os.path.join(meson_current_source_dir, 'Cargo.toml'),
             '--workspace']
if target == 'release':
    cargo_cmd.append('--release')

for e in exclude.split(','):
    cargo_cmd.append('--exclude')
    cargo_cmd.append(e)

try:
    subprocess.run(cargo_cmd, env=env, check=True)
except subprocess.SubprocessError:
    sys.exit(1)

# Copy so files to build dir
for f in glob.glob(os.path.join(cargo_target_dir, target, '*.' + ext)):
    shutil.copy(f, meson_build_dir)
