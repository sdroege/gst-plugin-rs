project('gst-plugins-rs',
        'rust',
        'c',
        version: '0.13.0',
        meson_version : '>= 0.56')

if get_option('debug')
  target = 'debug'
else
  target = 'release'
endif

cargo = find_program('cargo', version:'>=1.40')
cargo_wrapper = find_program('cargo_wrapper.py')
cargo_c = find_program('cargo-cbuild', version:'>=0.9.3', required: false)
rustc = find_program('rustc', version:'>=1.52')

if not cargo_c.found()
  error('cargo-c missing, install it with: \'cargo install cargo-c\'')
endif

system = build_machine.system()
if system == 'windows'
  ext_dynamic = 'dll'
  ext_static = 'lib'
elif system == 'darwin'
  ext_dynamic = 'dylib'
  ext_static = 'a'
else
  ext_dynamic = 'so'
  ext_static = 'a'
endif

# workspace name -> lib name
plugins = {
  'gst-plugin-audiofx': 'libgstrsaudiofx',
  'gst-plugin-cdg': 'libgstcdg',
  'gst-plugin-claxon': 'libgstclaxon',
  'gst-plugin-fallbackswitch': 'libgstfallbackswitch',
  'gst-plugin-ffv1': 'libgstffv1',
  'gst-plugin-file': 'libgstrsfile',
  'gst-plugin-flavors': 'libgstrsflv',
  'gst-plugin-gif': 'libgstgif',
  'gst-plugin-lewton': 'libgstlewton',
  'gst-plugin-rav1e': 'libgstrav1e',
  'gst-plugin-reqwest': 'libgstreqwest',
  'gst-plugin-hlssink3': 'libgsthlssink3',
  'gst-plugin-rspng': 'libgstrspng',
  'gst-plugin-rusoto': 'libgstrusoto',
  'gst-plugin-textwrap': 'libgstrstextwrap',
  'gst-plugin-fmp4': 'libgstfmp4',
  'gst-plugin-threadshare': 'libgstthreadshare',
  'gst-plugin-togglerecord': 'libgsttogglerecord',
  'gst-plugin-hsv': 'libgsthsv',
  'gst-plugin-json': 'libgstrsjson',
  'gst-plugin-regex': 'libgstrsregex',
  # FIXME: libwebp-sys2 will build its bundled version on msvc and apple platforms
  # https://github.com/qnighy/libwebp-sys2-rs/issues/4
  'gst-plugin-webp': 'libgstrswebp',
  'gst-plugin-videofx': 'libgstvideofx',
  'gst-plugin-uriplaylistbin': 'libgsturiplaylistbin',
  'gst-plugin-spotify': 'libgstspotify',
  'gst-plugin-textahead': 'libgsttextahead',
  'gst-plugin-onvif': 'libgstrsonvif',
}

extra_env = {}

if dependency('pangocairo', required : get_option('closedcaption')).found()
  plugins += {'gst-plugin-closedcaption' : 'libgstrsclosedcaption',}
endif

if dependency('dav1d', required : get_option('dav1d')).found()
  plugins += {'gst-plugin-dav1d' : 'libgstrsdav1d'}
endif

sodium = get_option ('sodium')
if sodium == 'system'
  dependency('libsodium')
  plugins += {'gst-plugin-sodium': 'libgstsodium'}
  extra_env += {'SODIUM_USE_PKG_CONFIG': '1'}
elif sodium == 'built-in'
  plugins += {'gst-plugin-sodium': 'libgstsodium'}
endif

cc = meson.get_compiler('c')
csound_option = get_option('csound')
# try first to find csound using pkg-config
csound_dep = dependency('', required: false)
if not csound_dep.found() and not csound_option.disabled()
  # if csound isn't distributed with pkg-config then user needs to define CSOUND_LIB_DIR with its location
  python3 = import('python').find_installation()
  res = run_command(python3, '-c', 'import os; print(os.environ["CSOUND_LIB_DIR"])')
  if res.returncode() == 0
    csound_libdir = res.stdout().strip()
    csound_dep = cc.find_library('csound64', dirs: csound_libdir, required: false)
    if csound_dep.found()
      extra_env += {'CSOUND_LIB_DIR': csound_libdir}
    endif
  endif
endif

if csound_dep.found()
  plugins += {'gst-plugin-csound' : 'libgstcsound'}
elif csound_option.enabled()
  error('csound option is enabled, but csound64 library could not be found and CSOUND_LIB_DIR was not set')
else
  message('csound not found, disabling its plugin')
endif

if dependency('gtk4', required : get_option('gtk4')).found()
  plugins += {'gst-plugin-gtk4' : 'libgstgtk4',}
endif

output = []

extensions = []

# Add the plugin file as output
if get_option('default_library') == 'shared' or get_option('default_library') == 'both'
  extensions += [ext_dynamic]
  foreach p, lib : plugins
    output += [lib + '.' + ext_dynamic]
  endforeach
endif

if get_option('default_library') == 'static' or get_option('default_library') == 'both'
  extensions += [ext_static]
  foreach p, lib : plugins
    output += [lib + '.' + ext_static]
  endforeach
endif

pc_files = []
foreach p, lib : plugins
  # skip the 'lib' prefix in plugin name
  pc_files += [lib.substring(3) + '.pc']
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

include = ','.join(plugins.keys())

# serialize extra_env
extra_env_list = []
foreach key, value : extra_env
  extra_env_list += key + ':' + value
endforeach
extra_env_str = ','.join(extra_env_list)

plugins_install_dir = get_option('libdir') / 'gstreamer-1.0'
pkgconfig_install_dir = get_option('libdir') / 'pkgconfig'


python = import('python').find_installation()
rs_plugins = custom_target('gst-plugins-rs',
  build_by_default: true,
  output: output,
  console: true,
  install: true,
  install_dir: plugins_install_dir,
  depends: depends,
  depfile: 'gst-plugins-rs.dep',
  command: [cargo_wrapper,
    'build',
    meson.current_build_dir(),
    meson.current_source_dir(),
    meson.build_root(),
    target,
    include,
    extra_env_str,
    get_option('prefix'),
    get_option('libdir'),
    '--depfile', '@DEPFILE@',
    '--exts', extensions,
  ])

plugins = rs_plugins.to_list()

# We don't need to pass a command as we depends on the target above
# but it is currently mandatory ( https://github.com/mesonbuild/meson/issues/8059 )
# so use python as it's guaranteed to be present on any setup
custom_target('gst-plugins-rs-pc-files',
  build_by_default: true,
  output: pc_files,
  console: true,
  install: true,
  install_dir: pkgconfig_install_dir,
  depends: rs_plugins,
  command: [python, '-c', '""'])

test('tests',
  cargo_wrapper,
  args: ['test',
    meson.current_build_dir(),
    meson.current_source_dir(),
    meson.build_root(),
    target,
    include,
    extra_env_str,
    get_option('prefix'),
    get_option('libdir')],
  timeout: 600)
