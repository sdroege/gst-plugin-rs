use chrono::TimeZone;
use git2::{Commit, ObjectType, Repository};
use std::path;

pub fn get_info() {
    let mut commit_id = "UNKNOWN".to_string();
    let mut commit_date = chrono::Utc::now().format("%Y-%m-%d").to_string();

    let mut repo_dir = path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    repo_dir.pop();

    let repo = Repository::open(&repo_dir);

    if let Ok(repo) = repo {
        let commit = find_last_commit(&repo).expect("Couldn't find last commit");
        commit_id = oid_to_short_sha(commit.id());
        let timestamp = commit.time().seconds();
        let dt = chrono::Utc.timestamp(timestamp, 0);
        commit_date = dt.format("%Y-%m-%d").to_string()
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
