plugins = []
rs_plugins = []
if output.length() > 0
  rs_plugins = custom_target('gst-plugins-rs',
    build_by_default: true,
    output: output,
    console: true,
    install: true,
    install_dir: plugins_install_dir,
    depends: depends,
    depfile: 'gst-plugins-rs.dep',
    env: extra_env,
    command: [cargo_wrapper,
      'build',
      meson.current_build_dir(),
      meson.current_source_dir() / '..',
      meson.global_build_root(),
      target,
      get_option('prefix'),
      get_option('libdir'),
      '--packages', packages,
      '--depfile', '@DEPFILE@',
      '--lib-suffixes', library_suffixes,
      '--target-dir', meson.current_build_dir() / '..' / 'target',
    ] + feature_args + extra_args)
  plugins = rs_plugins.to_list()
endif
