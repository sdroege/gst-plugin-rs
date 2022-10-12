project('gst-plugins-rs',
        'rust',
        'c',
        version: '0.9.0',
        meson_version : '>= 0.60')

python = import('python').find_installation()
fs = import('fs')

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
ext_exe = ''
if system == 'windows'
  ext_exe = 'exe'
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
# kept in the same order as the `members` list in Cargo.toml
plugins = {
  'gst-plugin-audiofx': 'libgstrsaudiofx',
  'gst-plugin-claxon': 'libgstclaxon',
  # csound has an external dependency, see below
  'gst-plugin-lewton': 'libgstlewton',
  'gst-plugin-spotify': 'libgstspotify',

  'gst-plugin-file': 'libgstrsfile',
  # sodium has an external dependency, see below
  'gst-plugin-threadshare': 'libgstthreadshare',

  'gst-plugin-mp4': 'libgstmp4',
  'gst-plugin-fmp4': 'libgstfmp4',

  'gst-plugin-aws': 'libgstaws',
  'gst-plugin-hlssink3': 'libgsthlssink3',
  'gst-plugin-ndi': 'libgstndi',
  'gst-plugin-raptorq': 'libgstraptorq',
  'gst-plugin-reqwest': 'libgstreqwest',
  'gst-plugin-rtp': 'libgstrsrtp',
  'gst-plugin-webrtchttp': 'libgstwebrtchttp',
  'gst-plugin-webrtc': 'libgstrswebrtc',

  'gst-plugin-textahead': 'libgsttextahead',
  'gst-plugin-json': 'libgstjson',
  'gst-plugin-regex': 'libgstregex',
  'gst-plugin-textwrap': 'libgsttextwrap',

  'gst-plugin-fallbackswitch': 'libgstfallbackswitch',
  'gst-plugin-livesync': 'libgstlivesync',
  'gst-plugin-togglerecord': 'libgsttogglerecord',
  'gst-plugin-tracers': 'libgstrstracers',
  'gst-plugin-uriplaylistbin': 'libgsturiplaylistbin',

  'gst-plugin-cdg': 'libgstcdg',
  # closedcaption has an external dependency, see below
  # dav1d has an external dependency, see below
  'gst-plugin-ffv1': 'libgstffv1',
  'gst-plugin-flavors': 'libgstrsflv',
  'gst-plugin-gif': 'libgstgif',
  # gtk4 has an external dependency, see below
  'gst-plugin-hsv': 'libgsthsv',
  'gst-plugin-png': 'libgstrspng',
  'gst-plugin-rav1e': 'libgstrav1e',
  # videofx has an external dependency, see below

  # FIXME: libwebp-sys2 will build its bundled version on msvc and apple platforms
  # https://github.com/qnighy/libwebp-sys2-rs/issues/4
  'gst-plugin-webp': 'libgstrswebp',
}

extra_env = {}

if dependency('cairo-gobject', required : get_option('videofx')).found()
  plugins += {'gst-plugin-videofx': 'libgstrsvideofx',}
endif

if dependency('pangocairo', required : get_option('closedcaption')).found()
  plugins += {'gst-plugin-closedcaption' : 'libgstrsclosedcaption',}
endif

if dependency('pangocairo', required : get_option('onvif')).found()
  plugins += {'gst-plugin-onvif': 'libgstrsonvif',}
endif

if dependency('dav1d', version : '>= 1.0.0', required : get_option('dav1d')).found()
  plugins += {'gst-plugin-dav1d' : 'libgstdav1d'}
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

  res = run_command(python, '-c', 'import os; print(os.environ["CSOUND_LIB_DIR"])', check: false)
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

if dependency('gtk4', version: '>= 4.6.0', required : get_option('gtk4')).found()
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

static_build = get_option('default_library') == 'static'
if static_build or get_option('default_library') == 'both'
  extensions += [ext_static]
  foreach p, lib : plugins
    output += [lib + '.' + ext_static]
  endforeach
endif

# Need to depends on all gstreamer-rs deps to ensure they are built
# before gstreamer-rs when building with gst-build.
# Custom targets can't depend on dependency() objects so we have to depend
# on the library variable from the subproject instead.
gst_req = '>= 1.18.0'
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
  ['gstreamer-sdp-1.0', 'gst-plugins-base', 'sdp_dep', 'gstsdp'],
  ['gstreamer-webrtc-1.0', 'gst-plugins-bad', 'gstwebrtc_dep', 'gstwebrtc'],
]

# Used to not lookup the same dependency multiple times which clutters logs
deps_cache = {}

foreach d: deps
  dep = dependency(d[0], version : gst_req,
    fallback : [d[1], d[2]])
  set_variable(d[2], dep)
  deps_cache += {d[0]: dep}
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

if get_option('doc').disabled()
  disable_doc = ['--disable-doc']
else
  disable_doc = []
endif

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
    meson.global_build_root(),
    target,
    include,
    extra_env_str,
    get_option('prefix'),
    get_option('libdir'),
    '--depfile', '@DEPFILE@',
    '--exts', extensions,
  ] + disable_doc)

plugins = rs_plugins.to_list()

# This is used by GStreamer to static link Rust plugins into gst-full
gst_plugins = []
pc_files = []
plugin_names = []
foreach plugin : plugins
  # skip the 'lib' prefix and extension from plugin path
  plugin_name = fs.stem(plugin.full_path()).substring(3)

  # Extract plugin dependencies from their Cargo.toml file
  plugin_deps = []
  p = run_command('dependencies.py', meson.current_source_dir(), plugin_name,
    capture: true,
    check: true)
  foreach dep_name : p.stdout().strip().split(',')
    dep_name_version = dep_name.split('|')
    dep_name = dep_name_version.get(0)
    if dep_name_version.length() > 1
      extras = {'version': dep_name_version.get(1)}
    else
      extras = {}
    endif
    if deps_cache.has_key(dep_name)
      plugin_deps += deps_cache[dep_name]
    else
      dep = dependency(dep_name, required: false, kwargs: extras)
      plugin_deps += dep
      deps_cache += {dep_name: dep}
    endif
  endforeach

  dep = declare_dependency(
    link_with: plugin,
    dependencies: plugin_deps,
    variables: {'full_path': plugin.full_path()},
  )
  meson.override_dependency(plugin_name, dep)

  if static_build and plugin_name in ['gstcsound', 'gstthreadshare', 'gstgtk4']
    warning('Static plugin @0@ is known to fail. It will not be included in libgstreamer-full.'.format(plugin_name))
  else
    gst_plugins += dep

    pc_files += [plugin_name + '.pc']
    if plugin_name.startswith('gst')
      plugin_names += [plugin_name.substring(3)]
    else
      plugin_names += [plugin_name]
    endif
  endif
endforeach

subdir('docs')

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


custom_target('gst-webrtc-signalling-server',
  build_by_default: true,
  output: 'gst-webrtc-signalling-server',
  console: true,
  install: true,
  install_dir: get_option('bindir'),
  depfile: 'gst-webrtc-signalling-server.dep',
  command: [cargo_wrapper,
    'build',
    meson.current_build_dir(),
    meson.current_source_dir(),
    meson.global_build_root(),
    target,
    '',
    '',
    get_option('prefix'),
    get_option('libdir'),
    '--depfile', '@DEPFILE@',
    '--exts', ext_exe,
    '--bin', 'gst-webrtc-signalling-server'
  ])

test('tests',
  cargo_wrapper,
  args: ['test',
    meson.current_build_dir(),
    meson.current_source_dir(),
    meson.global_build_root(),
    target,
    include,
    extra_env_str,
    get_option('prefix'),
    get_option('libdir')],
  timeout: 600)

summary({
    'Plugins': plugin_names,
}, list_sep: ', ')
