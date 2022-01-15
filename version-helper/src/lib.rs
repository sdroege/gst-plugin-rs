// Copyright (C) 2019 Sajeer Ahamed <ahamedsajeer.15.15@cse.mrt.ac.lk>
// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the MIT license, see the LICENSE file or <http://opensource.org/licenses/MIT>
//
// SPDX-License-Identifier: MIT

//! Extracts release for [GStreamer](https://gstreamer.freedesktop.org) plugin metadata
//!
//! See [`get_info`](fn.get_info.html) for details.
//!
//! This function is supposed to be used as follows in the `build.rs` of a crate that implements a
//! plugin:
//!
//! ```rust,ignore
//! gst_plugin_version_helper::info();
//! ```
//!
//! Inside `lib.rs` of the plugin, the information provided by `get_info` are usable as follows:
//!
//! ```rust,ignore
//! gst::plugin_define!(
//!     the_plugin_name,
//!     env!("CARGO_PKG_DESCRIPTION"),
//!     plugin_init,
//!     concat!(env!("CARGO_PKG_VERSION"), "-", env!("COMMIT_ID")),
//!     "The Plugin's License",
//!     env!("CARGO_PKG_NAME"),
//!     env!("CARGO_PKG_NAME"),
//!     env!("CARGO_PKG_REPOSITORY"),
//!     env!("BUILD_REL_DATE")
//! );
//! ```

mod git;

use chrono::{Datelike, TimeZone};
use std::convert::TryInto;
use std::time::SystemTime;
use std::{env, fs, path};

/// Extracts release for GStreamer plugin metadata
///
/// Release information is first tried to be extracted from a git repository at the same
/// place as the `Cargo.toml`, or one directory up to allow for Cargo workspaces. If no
/// git repository is found, we assume this is a release.
///
/// - If extracted from a git repository, sets the `COMMIT_ID` environment variable to the short
///   commit id of the latest commit and the `BUILD_REL_DATE` environment variable to the date of the
///   commit.
///
/// - If not, `COMMIT_ID` will be set to the string `RELEASE` and the
///   `BUILD_REL_DATE` variable will be set to the mtime of `Cargo.toml`.
///
/// - If neither is possible, `COMMIT_ID` is set to the string `UNKNOWN` and `BUILD_REL_DATE` to the
///   current date.
///
pub fn info() {
    let crate_dir =
        path::PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"));
    let mut repo_dir = crate_dir.clone();

    // First check for a git repository in the manifest directory and if there
    // is none try one directory up in case we're in a Cargo workspace
    let git_info = git::repo_hash(&repo_dir).or_else(move || {
        repo_dir.pop();
        git::repo_hash(&repo_dir)
    });

    // If there is a git repository, extract the version information from there.
    // Otherwise assume this is a release and use Cargo.toml mtime as date.
    let (commit_id, commit_date) = git_info.unwrap_or_else(|| {
        let date = cargo_mtime_date(crate_dir).unwrap_or_else(chrono::Utc::now);
        ("RELEASE".into(), date.format("%Y-%m-%d").to_string())
    });

    println!("cargo:rustc-env=COMMIT_ID={}", commit_id);
    println!("cargo:rustc-env=BUILD_REL_DATE={}", commit_date);
}

fn cargo_mtime_date(crate_dir: path::PathBuf) -> Option<chrono::DateTime<chrono::Utc>> {
    let mut cargo_toml = crate_dir;
    cargo_toml.push("Cargo.toml");

    let metadata = fs::metadata(&cargo_toml).ok()?;
    let mtime = metadata.modified().ok()?;
    let unix_time = mtime.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    let dt = chrono::Utc.timestamp(unix_time.as_secs().try_into().ok()?, 0);

    // FIXME: Work around https://github.com/rust-lang/cargo/issues/10285
    if dt.date().year() < 2015 {
        return None;
    }

    Some(dt)
}
