use anyhow::Result;
use librespot_core::{
    authentication::Credentials, cache::Cache, config::SessionConfig, session::Session,
};
use librespot_oauth::OAuthClientBuilder;
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    println!("==================================================");
    println!("  GStreamer Spotify Plugin - Auth Helper");
    println!("==================================================");

    let session_config = SessionConfig::default();

    let oauth = OAuthClientBuilder::new(
        &session_config.client_id,     // Spotify's desktop app client ID.
        "http://127.0.0.1:8888/login", // Redirect URI associated with the above ID. Other hosts will not work.
        vec!["streaming"],
    )
    .open_in_browser()
    .build()?;

    let token = oauth.get_access_token()?;
    let credentials = Credentials::with_access_token(&token.access_token);
    println!("Obtained streaming access token: {}", token.access_token);

    let creds_path = std::env::args().nth(1).map(PathBuf::from);

    let cache = if let Some(ref path) = creds_path {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            println!("Creating missing directories for {}", path.display());
            std::fs::create_dir_all(parent)?;
        }
        Some(Cache::new(Some(path.clone()), None, None, None)?)
    } else {
        None
    };

    let session = Session::new(session_config, cache);
    session.connect(credentials, true).await?;

    if let Some(path) = creds_path {
        println!(
            "Credentials file for user '{}' stored in {}",
            session.username(),
            path.display()
        );
    }

    Ok(())
}
