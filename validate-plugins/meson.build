validate_plugins_install_dir = plugins_install_dir / 'validate'
if not validate_option.allowed()
  subdir_done()
endif

# Check for gstreamer-validate dependency
gstvalidate_dep = dependency('gstreamer-validate-1.0', required: validate_option)

# Build the validate plugin
lib = 'libgstrsvalidate'
libname = 'gstrsvalidate'
if cc.get_argument_syntax() == 'msvc'
  lib = libname
endif

validate_output = []
validate_install_dirs = []
if default_library in ['shared', 'both']
  validate_output += [lib + '.' + ext_dynamic]
  validate_install_dirs += [validate_plugins_install_dir]
endif
if default_library in ['static', 'both']
  validate_output += [lib + '.' + ext_static]
  validate_install_dirs += [validate_plugins_install_dir]
endif

if not disable_pc_generation
  validate_output += [libname + '.pc']
  validate_install_dirs += [pkgconfig_install_dir]
endif

# Build the validate plugin
rs_validate_plugins = custom_target('gst-validate-plugins-rs',
  build_by_default: true,
  output: validate_output,
  console: true,
  install: true,
  install_dir: validate_install_dirs,
  depends: depends,
  depfile: 'gst-validate-plugins-rs.dep',
  env: extra_env,
  command: [cargo_wrapper,
    'build',
    meson.current_build_dir(),
    meson.current_source_dir() / '..',
    meson.global_build_root(),
    target,
    get_option('prefix'),
    get_option('libdir'),
    '--packages', 'gst-plugin-validate',
    '--depfile', '@DEPFILE@',
    '--lib-suffixes', library_suffixes,
    '--target-dir', meson.current_build_dir() / '..' / 'target',
  ])

# Set up development environment for validate plugins
meson.add_devenv({'GST_VALIDATE_PLUGIN_PATH': meson.current_build_dir()}, method: 'prepend')

# Run validate tests if tests are enabled
if get_option('tests').allowed() and validate_option.allowed()
  gst_tester = find_program('gst-tester-1.0', required: false)
  if gst_tester.found()
    tests = [
      'check_qrcode_content',
      'check_qrcode_fields',
    ]

    test_env = environment()
    test_env.set('GST_VALIDATE_PLUGIN_PATH', meson.current_build_dir())
    test_env.set('GST_REGISTRY', '@0@/@1@.registry'.format(meson.current_build_dir(), 'validate-rs'))

    test_dir = meson.current_source_dir() / '..' / 'utils' / 'validate' / 'tests'

    foreach test_name : tests
      test_file = join_paths(test_dir, test_name + '.validatetest')
      test('validate.rs.' + test_name,
        gst_tester,
        args: [test_file, '--use-fakesinks'],
        env: test_env,
        depends: rs_validate_plugins,
        timeout: 60,
        protocol: 'tap')
    endforeach
  endif
endif
