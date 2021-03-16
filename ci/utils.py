import os

DIRS = ['audio', 'generic', 'net', 'text', 'utils', 'video']
# Plugins whose name is prefixed by 'rs'
RS_PREFIXED = ['audiofx', 'closedcaption', 'dav1d', 'file', 'json', 'regex', 'webp']
OVERRIDE = {'wrap': 'rstextwrap', 'flavors': 'rsflv'}


def iterate_plugins():
    for d in DIRS:
        for name in os.listdir(d):
            if name in RS_PREFIXED:
                name = "rs{}".format(name)
            else:
                name = OVERRIDE.get(name, name)
            yield name
