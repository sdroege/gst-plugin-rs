extern crate gcc;
extern crate pkg_config;

fn main() {
    let gstreamer = pkg_config::probe_library("gstreamer-1.0").unwrap();
    let gstbase = pkg_config::probe_library("gstreamer-base-1.0").unwrap();
    let includes = [gstreamer.include_paths, gstbase.include_paths];

    let files = ["src/plugin.c", "src/rsfilesrc.c"];

    let mut config = gcc::Config::new();
    config.include("src");

    for f in files.iter() {
        config.file(f);
    }

    for p in includes.iter().flat_map(|i| i) {
        config.include(p);
    }

    config.compile("librsplugin-c.a");
}
