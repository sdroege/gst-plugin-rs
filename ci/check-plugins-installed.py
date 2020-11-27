#!/usr/bin/env python3
# Check that all available plugins have been build and installed in the prefix
# directory passed in argument.
import sys
import os
import glob

from utils import iterate_plugins

prefix = sys.argv[1]

plugins = glob.glob(os.path.join(
    prefix, '**', 'gstreamer-1.0', '*.so'), recursive=True)
plugins = list(map(os.path.basename, plugins))
print("Built plugins:", plugins)

success = True

for name in iterate_plugins():
    plugin = "libgst{}.so".format(name)

    if plugin not in plugins:
        print(name, "missing in", prefix)
        success = False

if not success:
    sys.exit(1)
