// SPDX-License-Identifier: MPL-2.0

fn main() {
    gst_plugin_version_helper::info();

    if std::env::var_os("CARGO_FEATURE_WEB_SERVER_EMBEDDED").is_some() {
        web_assets::build();
    }
}

/// Builds the gstwebrtc-api web page with npm and generates a table of the
/// produced files so they can be embedded into the plugin and served by the
/// webrtcsink web server (`web_server_embedded` feature).
mod web_assets {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::{env, fs};

    #[cfg(windows)]
    const DEFAULT_NPM: &str = "npm.cmd";
    #[cfg(not(windows))]
    const DEFAULT_NPM: &str = "npm";

    /// npm build outputs, never staged (the .gitignore of gstwebrtc-api)
    const BUILD_OUTPUTS: &[&str] = &["node_modules", "dist", "docs"];

    pub fn build() {
        let out_dir = PathBuf::from(env::var_os("OUT_DIR").unwrap());

        // Opt-out for builds that enable every feature but have no npm, typically
        // `cargo build --all-features` in CI: the plugin is built without the page
        // and the web server refuses to start unless web-server-directory is set
        println!("cargo:rerun-if-env-changed=GST_WEBRTC_NO_EMBED_WEB_ASSETS");
        if env::var_os("GST_WEBRTC_NO_EMBED_WEB_ASSETS").is_some() {
            fs::write(out_dir.join("web_server_embedded_assets.rs"), "&[]\n").unwrap();
            return;
        }

        // npm from the NPM env var, or looked up in PATH
        println!("cargo:rerun-if-env-changed=NPM");
        let npm_cmd = env::var("NPM").unwrap_or_else(|_| DEFAULT_NPM.to_string());

        let api_dir =
            PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("gstwebrtc-api");

        // Stage the sources in OUT_DIR so the source tree stays pristine
        let staging = out_dir.join("gstwebrtc-api");
        if staging.exists() {
            fs::remove_dir_all(&staging).unwrap();
        }
        fs::create_dir_all(&staging).unwrap();
        let entries = fs::read_dir(&api_dir)
            .unwrap_or_else(|err| panic!("cannot read {}: {err}", api_dir.display()));
        for entry in entries {
            let entry = entry.unwrap();
            let name = entry.file_name();
            let name = name.to_str().unwrap();
            if BUILD_OUTPUTS.contains(&name) || name.ends_with(".tgz") {
                continue;
            }
            println!("cargo:rerun-if-changed={}", entry.path().display());
            if entry.file_type().unwrap().is_dir() {
                fs_extra::dir::copy(entry.path(), &staging, &fs_extra::dir::CopyOptions::new())
                    .unwrap();
            } else {
                fs::copy(entry.path(), staging.join(name)).unwrap();
            }
        }

        npm(
            &npm_cmd,
            &staging,
            &["install", "--no-audit", "--no-fund", "--loglevel=error"],
        );
        npm(&npm_cmd, &staging, &["run", "build"]);

        let mut files: Vec<PathBuf> = fs::read_dir(staging.join("dist"))
            .unwrap()
            .map(|entry| entry.unwrap())
            .filter(|entry| entry.file_type().unwrap().is_file())
            .map(|entry| entry.path())
            .collect();
        files.sort();

        let entries: String = files
            .iter()
            .map(|path| {
                format!(
                    "    (\"{}\", include_bytes!(r\"{}\") as &[u8]),\n",
                    path.file_name().unwrap().to_str().unwrap(),
                    path.display()
                )
            })
            .collect();
        fs::write(
            out_dir.join("web_server_embedded_assets.rs"),
            format!("&[\n{entries}]\n"),
        )
        .unwrap();
    }

    fn npm(npm_cmd: &str, dir: &Path, args: &[&str]) {
        let status = Command::new(npm_cmd)
            .args(args)
            .current_dir(dir)
            .status()
            .unwrap_or_else(|err| {
                panic!(
                    "failed to run {npm_cmd}: {err}, the web_server_embedded feature \
                     requires npm (its path can be set through the NPM env var, \
                     GST_WEBRTC_NO_EMBED_WEB_ASSETS builds the plugin without the page)"
                )
            });
        if !status.success() {
            panic!("`{npm_cmd} {}` failed in {}", args.join(" "), dir.display());
        }
    }
}
