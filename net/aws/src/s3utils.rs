// Copyright (C) 2017 Author: Arun Raghavan <arun@arunraghavan.net>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use aws_config::identity::IdentityCache;
use aws_config::meta::region::RegionProviderChain;
use aws_credential_types::provider::{self, ProvideCredentials, error::CredentialsError};
use aws_sdk_s3::{
    config::{Credentials, Region, StalledStreamProtectionConfig, timeout::TimeoutConfig},
    error::{DisplayErrorContext, ProvideErrorMetadata},
    primitives::{ByteStream, ByteStreamError},
};
use aws_types::sdk_config::SdkConfig;

use gst::glib;
use gst::prelude::*;

use bytes::{Bytes, BytesMut, buf::BufMut};
use futures::{Future, future};
use std::fmt;
use std::sync::LazyLock;
use std::sync::Mutex;
use std::time::{Duration, UNIX_EPOCH};
use tokio::runtime;

pub const DEFAULT_S3_REGION: &str = "us-west-2";

pub static AWS_BEHAVIOR_VERSION: LazyLock<aws_config::BehaviorVersion> =
    LazyLock::new(aws_config::BehaviorVersion::v2026_01_12);

pub static RUNTIME: LazyLock<runtime::Runtime> = LazyLock::new(|| {
    runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .thread_name("gst-aws-runtime")
        .build()
        .unwrap()
});

#[derive(Debug)]
pub enum WaitError<E> {
    Cancelled,
    FutureError(E),
}

impl<E: ProvideErrorMetadata + std::error::Error> fmt::Display for WaitError<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WaitError::Cancelled => f.write_str("Cancelled"),
            WaitError::FutureError(err) => {
                write!(f, "{}: {}", DisplayErrorContext(&err), err.meta())
            }
        }
    }
}

#[derive(Default)]
pub enum Canceller {
    #[default]
    None,
    Handle(future::AbortHandle),
    Cancelled,
}

impl Canceller {
    pub fn abort(&mut self) {
        if let Canceller::Handle(ref canceller) = *self {
            canceller.abort();
        }

        *self = Canceller::Cancelled;
    }
}

pub fn wait<F, T, E>(canceller_mutex: &Mutex<Canceller>, future: F) -> Result<T, WaitError<E>>
where
    F: Send + Future<Output = Result<T, E>>,
    F::Output: Send,
    T: Send,
    E: Send,
{
    let mut canceller = canceller_mutex.lock().unwrap();
    if matches!(*canceller, Canceller::Cancelled) {
        return Err(WaitError::Cancelled);
    }
    let (abort_handle, abort_registration) = future::AbortHandle::new_pair();
    *canceller = Canceller::Handle(abort_handle);
    drop(canceller);

    let abortable_future = future::Abortable::new(future, abort_registration);

    // FIXME: add a timeout as well

    let res = {
        RUNTIME.block_on(async {
            match abortable_future.await {
                // Future resolved successfully
                Ok(Ok(res)) => Ok(res),
                // Future resolved with an error
                Ok(Err(err)) => Err(WaitError::FutureError(err)),
                // Canceller called before future resolved
                Err(future::Aborted) => Err(WaitError::Cancelled),
            }
        })
    };

    /* Clear out the canceller */
    let mut canceller = canceller_mutex.lock().unwrap();
    if matches!(*canceller, Canceller::Cancelled) {
        return Err(WaitError::Cancelled);
    }
    *canceller = Canceller::None;
    drop(canceller);

    res
}

pub fn wait_stream(
    canceller_mutex: &Mutex<Canceller>,
    stream: &mut ByteStream,
) -> Result<Bytes, WaitError<ByteStreamError>> {
    wait(canceller_mutex, async move {
        let mut collect = BytesMut::new();

        // Loop over the stream and collect till we're done
        while let Some(item) = stream.try_next().await? {
            collect.put(item)
        }

        Ok::<Bytes, ByteStreamError>(collect.freeze())
    })
}

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "awscredentials",
        gst::DebugColorFlags::empty(),
        Some("AWS credentials provider"),
    )
});

/// Builds `Credentials` from the `gst::Structure` returned by a
/// `request-credentials` signal handler.
///
/// Required fields: `access-key` (string), `secret-access-key` (string).
/// Optional fields: `session-token` (string) and `expiration` (u64, Unix
/// epoch seconds).
pub fn credentials_from_structure(
    structure: &gst::Structure,
    provider_name: &'static str,
) -> Option<Credentials> {
    let access_key = structure.get::<String>("access-key").ok()?;
    let secret_access_key = structure.get::<String>("secret-access-key").ok()?;
    let session_token = structure.get_optional::<String>("session-token").ok()?;
    let expiration = structure.get_optional::<u64>("expiration").ok()?;

    let expires_after = match expiration {
        Some(seconds) => Some(UNIX_EPOCH.checked_add(Duration::from_secs(seconds))?),
        None => None,
    };

    Some(Credentials::new(
        access_key,
        secret_access_key,
        session_token,
        expires_after,
        provider_name,
    ))
}

/// A credentials provider that asks the application for credentials by
/// emitting the element's `request-credentials` signal.
///
/// The SDK's lazy identity cache invokes it shortly before the previously
/// returned credentials expire, so applications can hand out short-lived
/// (e.g. STS) tokens and refresh them mid-transfer. The credentials obtained
/// by the element when starting serve the cache's first resolution directly,
/// so the signal is not re-emitted right after start. If the signal is not
/// handled, the last known credentials (initially the ones from the element
/// properties) are used.
pub struct SignalCredentialsProvider {
    element: glib::WeakRef<gst::Element>,
    initial_credentials: Mutex<Option<Credentials>>,
    last_credentials: Mutex<Option<Credentials>>,
    provider_name: &'static str,
}

impl SignalCredentialsProvider {
    pub fn new(
        element: &gst::Element,
        initial_credentials: Credentials,
        provider_name: &'static str,
    ) -> Self {
        SignalCredentialsProvider {
            element: element.downgrade(),
            initial_credentials: Mutex::new(Some(initial_credentials.clone())),
            last_credentials: Mutex::new(Some(initial_credentials)),
            provider_name,
        }
    }

    async fn fetch_credentials(&self) -> provider::Result {
        // The element already obtained credentials when starting (via the
        // initial signal emission or the credential properties); hand them to
        // the cache's first resolution instead of emitting the signal again.
        if let Some(credentials) = self.initial_credentials.lock().unwrap().take() {
            return Ok(credentials);
        }

        let Some(element) = self.element.upgrade() else {
            return Err(CredentialsError::not_loaded(
                "element was finalized while credentials were being resolved",
            ));
        };

        // The handler is application code and may block, e.g. on an STS
        // request. Running it on the blocking pool means only a pool thread is
        // tied up while the handler runs; this future stays suspended at the
        // .await below instead of blocking inside poll(), so the element's
        // canceller (unlock()) and the identity cache's load timeout can still
        // cancel the S3 request by dropping this future. That detaches the
        // emission: the handler finishes in the background and its result is
        // discarded.
        let (element, structure) = tokio::task::spawn_blocking(move || {
            let structure =
                element.emit_by_name::<Option<gst::Structure>>("request-credentials", &[]);
            (element, structure)
        })
        .await
        .map_err(CredentialsError::provider_error)?;

        match structure {
            Some(structure) => match credentials_from_structure(&structure, self.provider_name) {
                Some(credentials) => {
                    gst::debug!(
                        CAT,
                        obj = &element,
                        "Obtained credentials from request-credentials signal, expiry: {:?}",
                        credentials.expiry()
                    );
                    *self.last_credentials.lock().unwrap() = Some(credentials.clone());
                    Ok(credentials)
                }
                None => Err(CredentialsError::provider_error(
                    "invalid structure returned by request-credentials signal handler",
                )),
            },
            None => {
                gst::debug!(
                    CAT,
                    obj = &element,
                    "request-credentials signal not handled, using previous credentials"
                );
                self.last_credentials
                    .lock()
                    .unwrap()
                    .clone()
                    .ok_or_else(|| {
                        CredentialsError::not_loaded(
                            "request-credentials signal not handled and no credentials set",
                        )
                    })
            }
        }
    }
}

impl ProvideCredentials for SignalCredentialsProvider {
    fn provide_credentials<'a>(&'a self) -> provider::future::ProvideCredentials<'a>
    where
        Self: 'a,
    {
        provider::future::ProvideCredentials::new(self.fetch_credentials())
    }

    fn fallback_on_interrupt(&self) -> Option<Credentials> {
        self.last_credentials.lock().unwrap().clone()
    }
}

impl fmt::Debug for SignalCredentialsProvider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalCredentialsProvider")
            .field("provider_name", &self.provider_name)
            .finish_non_exhaustive()
    }
}

/// Creates the `request-credentials` signal shared by all elements.
///
/// The signal has no arguments and returns an optional `gst::Structure` with
/// credentials, see [`credentials_from_structure`] for the fields. The first
/// connected handler that returns a value wins.
pub fn request_credentials_signal() -> glib::subclass::Signal {
    glib::subclass::Signal::builder("request-credentials")
        .return_type::<Option<gst::Structure>>()
        .accumulator(|_hint, _acc, value| {
            // First signal handler wins
            std::ops::ControlFlow::Break(value.clone())
        })
        .build()
}

/// Asks the application for initial credentials by emitting the element's
/// `request-credentials` signal.
///
/// Must be called without any locks held as the handler may access element
/// properties. Returns `Ok(None)` if the signal was not handled.
pub fn request_credentials(
    element: &gst::Element,
    provider_name: &'static str,
) -> Result<Option<Credentials>, gst::ErrorMessage> {
    let Some(structure) =
        element.emit_by_name::<Option<gst::Structure>>("request-credentials", &[])
    else {
        return Ok(None);
    };

    match credentials_from_structure(&structure, provider_name) {
        Some(credentials) => {
            gst::debug!(
                CAT,
                obj = element,
                "Obtained credentials from request-credentials signal, expiry: {:?}",
                credentials.expiry()
            );
            Ok(Some(credentials))
        }
        None => Err(gst::error_msg!(
            gst::ResourceError::Settings,
            ["Invalid structure returned by request-credentials signal handler"]
        )),
    }
}

// See setting-timeouts example in aws-sdk-rust.
pub fn timeout_config(request_timeout: Duration) -> TimeoutConfig {
    TimeoutConfig::builder()
        .operation_attempt_timeout(request_timeout)
        .build()
}

/// Identity cache load timeout used by elements that don't have a
/// request-timeout property, see [`config_loader`].
pub const DEFAULT_CREDENTIALS_LOAD_TIMEOUT: Duration = Duration::from_secs(15);

/// Returns a `ConfigLoader` with the given credentials provider installed, if
/// any.
///
/// The identity cache's load timeout bounds credentials resolution, which
/// includes any request-credentials signal handler; `load_timeout` aligns it
/// with the element's request timeout instead of the SDK's 5 s default. An
/// infinite timeout is capped as very large timers can overflow the cache's
/// internal sleep.
pub fn config_loader<P: ProvideCredentials + 'static>(
    credentials: Option<P>,
    load_timeout: Option<Duration>,
) -> aws_config::ConfigLoader {
    match credentials {
        Some(cred) => {
            let mut identity_cache = IdentityCache::lazy();
            if let Some(timeout) = load_timeout {
                identity_cache =
                    identity_cache.load_timeout(timeout.min(Duration::from_secs(3600)));
            }
            aws_config::defaults(*AWS_BEHAVIOR_VERSION)
                .identity_cache(identity_cache.build())
                .credentials_provider(cred)
        }
        None => aws_config::defaults(*AWS_BEHAVIOR_VERSION),
    }
}

pub fn wait_config<P: ProvideCredentials + 'static>(
    canceller_mutex: &Mutex<Canceller>,
    region: Option<Region>,
    timeout_config: TimeoutConfig,
    credentials: Option<P>,
) -> Result<SdkConfig, WaitError<ByteStreamError>> {
    let region_provider = if let Some(region) = region {
        RegionProviderChain::first_try(region)
            .or_default_provider()
            .or_else(Region::new(DEFAULT_S3_REGION))
    } else {
        RegionProviderChain::default_provider()
    };

    // See https://github.com/awslabs/aws-sdk-rust/discussions/956
    // Stalled-stream protection is deliberately disabled to maintain
    // old behaviour. translate, transcriber and polly plugins already
    // disable this. We would also need to provide a property as this
    // needs to be specified by the user ideally.
    let config_future = config_loader(credentials, timeout_config.operation_attempt_timeout())
        .timeout_config(timeout_config)
        .region(region_provider)
        .stalled_stream_protection(StalledStreamProtectionConfig::disabled())
        .load();

    let mut canceller = canceller_mutex.lock().unwrap();
    if matches!(*canceller, Canceller::Cancelled) {
        return Err(WaitError::Cancelled);
    }
    let (abort_handle, abort_registration) = future::AbortHandle::new_pair();
    *canceller = Canceller::Handle(abort_handle);
    drop(canceller);

    let abortable_future = future::Abortable::new(config_future, abort_registration);

    let res = {
        RUNTIME.block_on(async {
            match abortable_future.await {
                // Future resolved successfully
                Ok(config) => Ok(config),
                // Canceller called before future resolved
                Err(future::Aborted) => Err(WaitError::Cancelled),
            }
        })
    };

    /* Clear out the canceller */
    let mut canceller = canceller_mutex.lock().unwrap();
    if matches!(*canceller, Canceller::Cancelled) {
        return Err(WaitError::Cancelled);
    }
    *canceller = Canceller::None;
    drop(canceller);

    res
}

pub fn duration_from_millis(millis: i64) -> Duration {
    match millis {
        -1 => Duration::MAX,
        v => Duration::from_millis(v as u64),
    }
}

pub fn duration_to_millis(dur: Option<Duration>) -> i64 {
    match dur {
        None => Duration::MAX.as_millis() as i64,
        Some(d) => d.as_millis() as i64,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            gst::init().unwrap();
        });
    }

    #[test]
    fn credentials_from_structure_full() {
        init();

        let structure = gst::Structure::builder("aws-credentials")
            .field("access-key", "key")
            .field("secret-access-key", "secret")
            .field("session-token", "token")
            .field("expiration", 1_700_000_000u64)
            .build();

        let credentials = credentials_from_structure(&structure, "test").unwrap();
        assert_eq!(credentials.access_key_id(), "key");
        assert_eq!(credentials.secret_access_key(), "secret");
        assert_eq!(credentials.session_token(), Some("token"));
        assert_eq!(
            credentials.expiry(),
            Some(UNIX_EPOCH + Duration::from_secs(1_700_000_000))
        );
    }

    #[test]
    fn credentials_from_structure_minimal() {
        init();

        let structure = gst::Structure::builder("aws-credentials")
            .field("access-key", "key")
            .field("secret-access-key", "secret")
            .build();

        let credentials = credentials_from_structure(&structure, "test").unwrap();
        assert_eq!(credentials.session_token(), None);
        assert_eq!(credentials.expiry(), None);
    }

    #[test]
    fn credentials_from_structure_invalid() {
        init();

        // Missing secret-access-key
        let structure = gst::Structure::builder("aws-credentials")
            .field("access-key", "key")
            .build();
        assert!(credentials_from_structure(&structure, "test").is_none());

        // Wrong type for expiration
        let structure = gst::Structure::builder("aws-credentials")
            .field("access-key", "key")
            .field("secret-access-key", "secret")
            .field("expiration", "not-a-number")
            .build();
        assert!(credentials_from_structure(&structure, "test").is_none());
    }
}
