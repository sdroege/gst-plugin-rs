fn main() {
    let gstreamer = pkg_config::probe_library("gstreamer-1.0").unwrap();
    let gstrtp = pkg_config::probe_library("gstreamer-rtp-1.0").unwrap();
    let includes = [gstreamer.include_paths, gstrtp.include_paths];

    let mut build = cc::Build::new();

    for p in includes.iter().flatten() {
        build.include(p);
    }

    build.file("src/jitterbuffer/rtpjitterbuffer.c");
    build.file("src/jitterbuffer/rtpstats.c");

    build.define("RTPJitterBuffer", "TsRTPJitterBuffer");
    build.define("RTPJitterBufferClass", "TsRTPJitterBufferClass");
    build.define("RTPJitterBufferPrivate", "TsRTPJitterBufferClass");

    build.compile("libthreadshare-c.a");

    println!("cargo:rustc-link-lib=dylib=gstrtp-1.0");

    for path in gstrtp.link_paths.iter() {
        println!(
            "cargo:rustc-link-search=native={}",
            path.to_str().expect("library path doesn't exist")
        );
    }

    gst_plugin_version_helper::info()
}
