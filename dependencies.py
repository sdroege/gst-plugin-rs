#!/usr/bin/env python3

# Parse Cargo.toml files for each plugin to collect their external dependencies.
# Meson will lookup those dependencies using pkg-config to be able to link
# static Rust plugins into gst-full.

from argparse import ArgumentParser
from pathlib import Path
import sys


try:
    # Python11 stdlib
    import tomllib
except ImportError:
    import tomli as tomllib


PARSER = ArgumentParser()
PARSER.add_argument('src_dir', type=Path)
PARSER.add_argument('plugins', nargs='*')


# Map plugin name to directory name, for those that does not match.
RENAMES = {
    'rsaudiofx': 'audiofx',
    'rsfile': 'file',
    'rsflv': 'flavors',
    'rsrtp': 'rtp',
    'rswebp': 'webp',
    'rsonvif': 'onvif',
    'rstracers': 'tracers',
    'rsclosedcaption': 'closedcaption',
    'rswebrtc': 'webrtc',
    'rspng': 'png',
    'rsvideofx': 'videofx',
    'textahead': 'ahead',
    'textwrap': 'wrap',
}


if __name__ == "__main__":
    opts = PARSER.parse_args()

    with (opts.src_dir / 'Cargo.toml').open('rb') as f:
        crates = tomllib.load(f)['workspace']['members']
    deps = set()
    for p in opts.plugins:
        assert p.startswith('gst')
        name = p[3:]
        name = RENAMES.get(name, name)
        crate_path = None
        for crate in crates:
            if Path(crate).name == name:
                crate_path = opts.src_dir / crate / 'Cargo.toml'
        assert crate_path
        with crate_path.open('rb') as f:
            data = tomllib.load(f)
            try:
                requires = data['package']['metadata']['capi']['pkg_config']['requires_private']
            except KeyError:
                continue
            deps.update([i.strip().replace('>', "|>").replace('<', "|<").replace("==", "|==") for i in requires.split(',')])
    print(','.join(deps))
