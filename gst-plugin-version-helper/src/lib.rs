// Copyright (C) 2019 Sajeer Ahamed <ahamedsajeer.15.15@cse.mrt.ac.lk>
// Copyright (C) 2019 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the MIT license, see the LICENSE file or <http://opensource.org/licenses/MIT>

use chrono::TimeZone;
use git2::{Commit, ObjectType, Repository};
use std::{fs, path};

pub fn get_info() {
    let mut commit_id = "UNKNOWN".into();
    let mut commit_date = chrono::Utc::now().format("%Y-%m-%d").to_string();

    let crate_dir = path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut repo_dir = crate_dir.clone();

    // First check for a git repository in the manifest directory and if there
    // is none try one directory up in case we're in a Cargo workspace
    let repo = Repository::open(&repo_dir).or_else(move |_| {
        repo_dir.pop();
        Repository::open(&repo_dir)
    });

    let mut release_file = crate_dir.clone();
    release_file.push("release.txt");

    // If there is a git repository, extract the version information from there.
    // Otherwise use a 'release.txt' file as fallback, or report the current
    // date and an unknown revision.
    if let Ok(repo) = repo {
        let commit = find_last_commit(&repo).expect("Couldn't find last commit");
        commit_id = oid_to_short_sha(commit.id());
        let timestamp = commit.time().seconds();
        let dt = chrono::Utc.timestamp(timestamp, 0);
        commit_date = dt.format("%Y-%m-%d").to_string()
    } else if let Ok(release_file) = fs::File::open(release_file) {
        let mut cargo_toml = crate_dir.clone();
        cargo_toml.push("Cargo.toml");

        let cargo_toml = fs::File::open(cargo_toml).expect("Can't open Cargo.toml");
        commit_date = read_release_date(cargo_toml, release_file);
        commit_id = "RELEASE".into();
    }

    println!("cargo:rustc-env=COMMIT_ID={}", commit_id);
    println!("cargo:rustc-env=BUILD_REL_DATE={}", commit_date);
}

fn find_last_commit(repo: &Repository) -> Result<Commit, git2::Error> {
    let obj = repo.head()?.resolve()?.peel(ObjectType::Commit)?;
    obj.into_commit()
        .map_err(|_| git2::Error::from_str("Couldn't find commit"))
}

fn oid_to_short_sha(oid: git2::Oid) -> String {
    oid.to_string()[..8].to_string()
}

fn read_release_date(mut cargo_toml: fs::File, mut release_file: fs::File) -> String {
    use std::io::Read;

    let mut content = String::new();
    release_file
        .read_to_string(&mut content)
        .expect("Failed to read version file");

    let mut lines = content.lines();
    let version = lines.next().expect("Failed to read version number");
    let date = lines.next().expect("Failed to read release date");

    let mut cargo_toml_content = String::new();
    cargo_toml
        .read_to_string(&mut cargo_toml_content)
        .expect("Failed to read `Cargo.toml`");

    let cargo_toml = cargo_toml_content
        .parse::<toml::Value>()
        .expect("Failed to parse `Cargo.toml`");
    let package = cargo_toml["package"]
        .as_table()
        .expect("Failed to find 'package' section in `Cargo.toml`");
    let cargo_version = package["version"]
        .as_str()
        .expect("Failed to read version from `Cargo.toml`");

    if cargo_version != version {
        panic!(
            "Version from 'version.txt' `{}` different from 'Cargo.toml' `{}`",
            version, cargo_version
        );
    }

    date.into()
}
