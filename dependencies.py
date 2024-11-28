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
PARSER.add_argument('--features', action="store_true", help="Get list of features to activate")
PARSER.add_argument('--gst-version', help="GStreamer version used")


# Map plugin name to directory name, for those that does not match.
RENAMES = {
    'rsaudiofx': 'audiofx',
    'rsfile': 'file',
    'rsflv': 'flavors',
    'rsrtp': 'rtp',
    'rsrtsp': 'rtsp',
    'rswebp': 'webp',
    'rsonvif': 'onvif',
    'rstracers': 'tracers',
    'rsclosedcaption': 'closedcaption',
    'rswebrtc': 'webrtc',
    'rspng': 'png',
    'rsvideofx': 'videofx',
    'rsinter': 'inter',
    'textahead': 'ahead',
    'textwrap': 'wrap',
}

class CargoAnalyzer:
    def __init__(self):
        self.src_dir = None
        self.plugins = None
        self.features = False
        self.gst_version = "1.18"

    def extract_version(self, feature_name):
        if feature_name.startswith('v'):
            verindex = 1
        elif feature_name.startswith('gst'):
            verindex = 3
        else:
            return None

        (majver, minver) = feature_name[verindex:].split("_")
        return (int(majver), int(minver))

    def extract_features(self, cargo_data):
        features = cargo_data['features']
        print(f'Available features: {features!r}', file=sys.stderr)
        wanted_features = set()
        gst_version_major = int(self.gst_version.split('.')[0])
        gst_version_minor = int(self.gst_version.split('.')[1])
        if gst_version_minor % 2:
            gst_version_minor += 1
        for (name, value) in features.items():
            version = self.extract_version(name)

            if version is None:
                continue
            (majver, minver) = version

            if gst_version_major < majver or gst_version_minor < minver:
                continue
            wanted_features |= set([name])
            wanted_features |= set(value)
            if name.startswith("gst"):
                # Required for some reason for rswebrtc which has a specific feature
                wanted_features |= {f"{cargo_data['package']['name']}/{name}"}

        print(f'Enabling features: {wanted_features!r}', file=sys.stderr)
        return wanted_features

    def run(self):
        with (opts.src_dir / 'Cargo.toml').open('rb') as f:
            crates = tomllib.load(f)['workspace']['members']
        res = set()
        for name in opts.plugins:
            if name.startswith('gst'):
                name = name[3:]

            name = RENAMES.get(name, name)
            crate_path = None
            for crate in crates:
                if Path(crate).name == name:
                    crate_path = opts.src_dir / crate / 'Cargo.toml'
            assert crate_path
            with crate_path.open('rb') as f:
                data = tomllib.load(f)
                if opts.features:
                    res |= self.extract_features(data)
                else:
                    try:
                        requires = data['package']['metadata']['capi']['pkg_config']['requires_private']
                    except KeyError:
                        continue
                    res.update([i.strip().replace('>', "|>").replace('<', "|<").replace("==", "|==") for i in requires.split(',')])
        return res

if __name__ == "__main__":
    analyzer = CargoAnalyzer()
    opts = PARSER.parse_args(namespace=analyzer)

    print(','.join(analyzer.run()))
