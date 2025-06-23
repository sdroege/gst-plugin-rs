import os
from warnings import warn

DIRS = [
    'audio',
    'generic',
    'mux',
    'net',
    'text',
    'utils',
    'video',
]

# Plugins whose name is prefixed by 'rs'
RS_PREFIXED = [
    'analytics',
    'audiofx',
    'closedcaption',
    'file',
    'onvif',
    'webp',
    'videofx',
    'webrtc',
    'png',
    'tracers',
    'rtp',
    'rtsp',
    'inter',
]

OVERRIDE = {
    'ahead': 'textahead',
    'flavors': 'rsflv',
    'wrap': 'textwrap',
}


def iterate_plugins():
    for d in DIRS:
        for name in os.listdir(d):
            if 'skia' in name:
                warn('Skipping skia, see https://gitlab.freedesktop.org/gstreamer/gst-plugins-rs/-/issues/692')
                continue
            if name in RS_PREFIXED:
                name = "rs{}".format(name)
            else:
                name = OVERRIDE.get(name, name)
            yield name
