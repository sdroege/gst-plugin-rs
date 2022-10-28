use futures::future;
use futures::prelude::*;
use gst::{prelude::*, ErrorMessage};
use once_cell::sync::Lazy;
use parse_link_header;
use reqwest::header::HeaderMap;
use reqwest::redirect::Policy;
use std::sync::Mutex;
use std::time::Duration;
use tokio::runtime;

pub static RUNTIME: Lazy<runtime::Runtime> = Lazy::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(1)
        .thread_name("webrtc-http-runtime")
        .build()
        .unwrap()
});

pub fn wait<F, T>(
    canceller: &Mutex<Option<future::AbortHandle>>,
    future: F,
    timeout: u32
) -> Result<T, ErrorMessage>
where
    F: Send + Future<Output = Result<T, ErrorMessage>>,
    T: Send + 'static,
{
    let mut canceller_guard = canceller.lock().unwrap();
    let (abort_handle, abort_registration) = future::AbortHandle::new_pair();

    if canceller_guard.is_some() {
        return Err(gst::error_msg!(
            gst::ResourceError::Failed,
            ["Old Canceller should not exist"]
        ));
    }

    canceller_guard.replace(abort_handle);
    drop(canceller_guard);

    let future = async {
        if timeout == 0 {
            future.await
        } else {
            let res = tokio::time::timeout(Duration::from_secs(timeout.into()), future).await;

            match res {
                Ok(r) => r,
                Err(e) => Err(gst::error_msg!(
                    gst::ResourceError::Read,
                    ["Request timeout, elapsed: {}", e.to_string()]
                )),
            }
        }
    };

    let future = async {
        match future::Abortable::new(future, abort_registration).await {
            Ok(Ok(res)) => Ok(res),

            Ok(Err(err)) => Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Future resolved with an error {}", err.to_string()]
            )),

            Err(future::Aborted) => Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Canceller called before future resolved"]
            )),
        }
    };

    let res = {
        let _enter = RUNTIME.enter();
        futures::executor::block_on(future)
    };

    canceller_guard = canceller.lock().unwrap();
    *canceller_guard = None;

    res
}

pub fn parse_redirect_location(
    headermap: &HeaderMap,
    old_url: &reqwest::Url,
) -> Result<reqwest::Url, ErrorMessage> {
    let location = match headermap.get(reqwest::header::LOCATION) {
        Some(location) => location,
        None => {
            return Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Location header field should be present for WHIP/WHEP resource URL"]
            ));
        }
    };

    let location = match location.to_str() {
        Ok(loc) => loc,
        Err(e) => {
            return Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Failed to convert location to string {}", e]
            ));
        }
    };

    match reqwest::Url::parse(location) {
        Ok(url) => Ok(url), // Location URL is an absolute path
        Err(_) => {
            // Location URL is a relative path
            let new_url = old_url.clone().join(location).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["URL join operation failed: {:?}", err]
                )
            })?;

            Ok(new_url)
        }
    }
}

pub fn build_reqwest_client(pol: Policy) -> reqwest::Client {
    let client_builder = reqwest::Client::builder();
    client_builder.redirect(pol).build().unwrap()
}

pub fn set_ice_servers(
    webrtcbin: &gst::Element,
    headermap: &HeaderMap,
) -> Result<(), ErrorMessage> {
    for link in headermap.get_all("link").iter() {
        let link = link.to_str().map_err(|err| {
            gst::error_msg!(
                gst::ResourceError::Failed,
                [
                    "Header value should contain only visible ASCII strings: {}",
                    err
                ]
            )
        })?;

        let item_map = match parse_link_header::parse_with_rel(link) {
            Ok(map) => map,
            Err(_) => continue,
        };

        let link = match item_map.contains_key("ice-server") {
            true => item_map.get("ice-server").unwrap(),
            false => continue, // Not a link header we care about
        };

        // Note: webrtcbin needs ice servers to be in the below format
        // <scheme>://<user:pass>@<url>
        // and the ice-servers (link headers) received from the whip server might be
        // in the format <scheme>:<host> with username and password as separate params.
        // Constructing these with 'url' crate also require a format/parse
        // for changing <scheme>:<host> to <scheme>://<user>:<password>@<host>.
        // So preferred to use the String rather

        let mut ice_server_url;

        // check if uri has ://
        if link.uri.has_authority() {
            // use raw_uri as is
            // username and password in the link.uri.params ignored
            ice_server_url = link.raw_uri.as_str().to_string();
        } else {
            // No builder pattern is provided by reqwest::Url. Use string operation.
            // construct url as '<scheme>://<user:pass>@<url>'
            ice_server_url = format!("{}://", link.uri.scheme());
            if let Some(user) = link.params.get("username") {
                ice_server_url += user.as_str();
                if let Some(pass) = link.params.get("credential") {
                    ice_server_url = ice_server_url + ":" + pass.as_str();
                }
                ice_server_url += "@";
            }

            // the raw_uri contains the ice-server in the form <scheme>:<url>
            // so strip the scheme and the ':' from the beginning of raw_uri and use
            // the rest of raw_uri to append it the url which will be in the form
            // <scheme>://<user:pass>@<url> as expected
            ice_server_url += link
                .raw_uri
                .strip_prefix((link.uri.scheme().to_owned() + ":").as_str())
                .expect("strip 'scheme:' from raw uri");
        }

        // It's nicer to not collapse the `else if` and its inner `if`
        #[allow(clippy::collapsible_if)]
        if link.uri.scheme() == "stun" {
            webrtcbin.set_property_from_str("stun-server", ice_server_url.as_str());
        } else if link.uri.scheme().starts_with("turn") {
            if !webrtcbin.emit_by_name::<bool>("add-turn-server", &[&ice_server_url.as_str()]) {
                return Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Failed to set turn server {}", ice_server_url]
                ));
            }
        }
    }

    Ok(())
}
