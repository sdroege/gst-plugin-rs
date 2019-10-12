project('gst-plugins-rs',
        'rust',
        version: '0.13.0',
        meson_version : '>= 0.52')

if get_option('debug')
  target = 'debug'
else
  target = 'release'
endif

cargo = find_program('cargo', version:'>=1.39')
cargo_script = find_program('cargo.py')

system = build_machine.system()
if system == 'windows'
  ext = 'dll'
elif system == 'darwin'
  ext = 'dylib'
else
  ext = 'so'
endif

plugins_rep = {
  'gst-plugin-audiofx': 'libgstrsaudiofx',
  'gst-plugin-cdg': 'libgstcdg',
  'gst-plugin-closedcaption': 'libgstrsclosedcaption',
  'gst-plugin-dav1d': 'libgstrsdav1d',
  'gst-plugin-fallbackswitch': 'libgstfallbackswitch',
  'gst-plugin-file': 'libgstrsfile',
  'gst-plugin-flv': 'libgstrsflv',
  'gst-plugin-lewton': 'libgstlewton',
  'gst-plugin-rav1e': 'libgstrav1e',
  'gst-plugin-reqwest': 'libgstreqwest',
  'gst-plugin-rusoto': 'libgstrusoto',
  'gst-plugin-sodium': 'libgstsodium',
  'gst-plugin-threadshare': 'libgstthreadshare',
  'gst-plugin-togglerecord': 'libgsttogglerecord',
}

output = []

foreach p, lib : plugins_rep
  # Add the plugin file as output
  output += [lib + '.' + ext]
endforeach

# Need to depends on all gstreamer-rs deps to ensure they are built
# before gstreamer-rs when building with gst-build.
# Custom targets can't depend on dependency() objects so we have to depend
# on the library variable from the subproject instead.
gst_req = '>= 1.14.0'
depends = []

deps = [
  # name, subproject name, subproject dep, library object
  ['gstreamer-1.0', 'gstreamer', 'gst_dep', 'libgst'],
  ['gstreamer-app-1.0', 'gst-plugins-base', 'app_dep', 'gstapp'],
  ['gstreamer-audio-1.0', 'gst-plugins-base', 'audio_dep', 'gstaudio'],
  ['gstreamer-base-1.0', 'gstreamer', 'gst_base_dep', 'gst_base'],
  ['gstreamer-check-1.0', 'gstreamer', 'gst_check_dep', 'gst_check'],
  ['gstreamer-net-1.0', 'gstreamer', 'gst_net_dep', 'gst_net'],
  ['gstreamer-rtp-1.0', 'gst-plugins-base', 'rtp_dep', 'gst_rtp'],
  ['gstreamer-video-1.0', 'gst-plugins-base', 'video_dep', 'gstvideo'],
]

foreach d: deps
  dep = dependency(d[0], version : gst_req,
    fallback : [d[1], d[2]])
  if dep.type_name() == 'internal'
    lib = subproject(d[1]).get_variable(d[3])
    depends += lib
  endif
endforeach

# Always build the target so we don't have to list all source files as input
rs_plugins = custom_target('gst-plugins-rs',
  build_by_default: true,
  output: output,
  console: true,
  install: false,
  build_always_stale: true,
  depends: depends,
  command: [cargo_script,
    meson.current_build_dir(),
    meson.current_source_dir(),
    meson.build_root(),
    target,
    ext])

# FIXME: raises a warning as the target has multiple outputs and meson will use
# only the first one. All the plugins have the same basedir, hence
# GST_PLUGIN_PATH will include them all, so that's ok.
plugins = [rs_plugins]
