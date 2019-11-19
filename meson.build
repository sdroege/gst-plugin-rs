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

# Always build the target so we don't have to list all source files as input
rs_plugins = custom_target('gst-plugins-rs',
  build_by_default: true,
  output: output,
  console: true,
  install: false,
  build_always_stale: true,
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
