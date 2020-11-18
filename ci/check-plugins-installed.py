#!/usr/bin/env python3
# Check that all available plugins have been build and installed in the prefix
# directory passed in argument.
import sys
import os
import glob

DIRS = ['audio', 'generic', 'net', 'text', 'utils', 'video']
OVERRIDE = {'wrap': 'textwrap', 'flavors': 'rsflv'}

prefix = sys.argv[1]

plugins = glob.glob(os.path.join(
    prefix, '**', 'gstreamer-1.0', '*.so'), recursive=True)
plugins = list(map(os.path.basename, plugins))
print("Built plugins:", plugins)

success = True

for d in DIRS:
    for name in os.listdir(d):
        name = OVERRIDE.get(name, name)

        plugin = "libgst{}.so".format(name)
        # Some plugins are prefixed with 'rs'
        rs_plugin = "libgstrs{}.so".format(name)

        if plugin not in plugins and rs_plugin not in plugins:
            print(name, "missing in", prefix)
            success = False

if not success:
    sys.exit(1)
