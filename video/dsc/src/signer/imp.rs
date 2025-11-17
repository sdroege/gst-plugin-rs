// Copyright (C) 2025, Fluendo S.A.
//      Author: Diego Nieto <dnieto@fluendo.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use glib::{ParamSpec, ParamSpecString, ParamSpecUInt, Value};

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst_base::subclass::prelude::*;

use rsa::RsaPrivateKey;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::pkcs1v15::SigningKey;
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{SignatureEncoding, Signer as _};
use sha1::Sha1;
use sha2::{Sha224, Sha256, Sha384, Sha512};

use std::fs;
use std::sync::{LazyLock, Mutex};

use anyhow::Result;

use crate::common::HashMethod;
use crate::dsc_substream::DscSubstreamManager;
use crate::nal_parser::{NalParser, VideoCodec};

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "dscsigner",
        gst::DebugColorFlags::empty(),
        Some("GstDscSigner"),
    )
});

#[derive(Default)]
struct GopSigningState {
    dsc_manager: Option<DscSubstreamManager>,
    gop_started: bool,
    buffers_in_substream: u32,
    nal_parser: Option<NalParser>,
    last_digest: Option<Vec<u8>>,
}

struct Settings {
    hash_method: HashMethod,
    private_key_path: Option<String>,
    public_key_uri: Option<String>,
    content_uuid: Option<[u8; 16]>,
    substream_length: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            hash_method: HashMethod::default(),
            private_key_path: None,
            public_key_uri: None,
            content_uuid: None,
            substream_length: 5,
        }
    }
}

#[derive(Default)]
struct State {
    private_key: Option<RsaPrivateKey>,
    gop_state: GopSigningState,
}

#[derive(Default)]
pub struct DscSigner {
    settings: Mutex<Settings>,
    state: Mutex<State>,
}

#[glib::object_subclass]
impl ObjectSubclass for DscSigner {
    const NAME: &'static str = "GstDscSigner";
    type Type = super::DscSigner;
    type ParentType = gst_base::BaseTransform;
}

impl ObjectImpl for DscSigner {
    fn set_property(&self, _id: usize, value: &Value, pspec: &ParamSpec) {
        match pspec.name() {
            "hash-method" => {
                let method = value.get::<HashMethod>().unwrap();
                self.settings.lock().unwrap().hash_method = method;
                gst::info!(CAT, "Set hash-method property to {method}");
            }
            "private-key-path" => {
                let path = value.get::<Option<String>>().unwrap();
                gst::info!(CAT, "Set private-key-path property to {path:?}");
                self.settings.lock().unwrap().private_key_path = path;
                self.state.lock().unwrap().private_key = None;
            }
            "public-key-uri" => {
                let uri = value.get::<Option<String>>().unwrap();
                gst::info!(CAT, "Set public-key-uri property to {uri:?}");
                self.settings.lock().unwrap().public_key_uri = uri;
            }
            "content-uuid" => {
                let uuid_str = value.get::<Option<String>>().unwrap();
                match uuid_str.as_deref() {
                    None | Some("") => {
                        gst::info!(CAT, "Unset content-uuid property");
                        self.settings.lock().unwrap().content_uuid = None;
                    }
                    Some(uuid_str) if uuid_str.len() == 32 => {
                        let mut uuid = [0u8; 16];
                        if hex::decode_to_slice(uuid_str.as_bytes(), &mut uuid).is_ok() {
                            gst::info!(CAT, "Set content-uuid property to {uuid_str}");
                            self.settings.lock().unwrap().content_uuid = Some(uuid);
                        } else {
                            gst::error!(CAT, "Invalid hex string for content-uuid: {uuid_str}");
                        }
                    }
                    Some(uuid_str) => {
                        gst::error!(
                            CAT,
                            "Content UUID must be 32 hex characters, got: {uuid_len}",
                            uuid_len = uuid_str.len()
                        );
                    }
                }
            }
            "substream-length" => {
                let length = value.get::<u32>().unwrap();
                self.settings.lock().unwrap().substream_length = length;
                gst::info!(CAT, "Set substream-length property to {length}");
            }
            _ => {}
        }
    }

    fn property(&self, _id: usize, pspec: &ParamSpec) -> Value {
        let settings = self.settings.lock().unwrap();
        match pspec.name() {
            "hash-method" => settings.hash_method.to_value(),
            "private-key-path" => settings.private_key_path.clone().to_value(),
            "public-key-uri" => settings.public_key_uri.clone().to_value(),
            "content-uuid" => settings.content_uuid.map(hex::encode).to_value(),
            "substream-length" => settings.substream_length.to_value(),
            _ => Value::from_type(pspec.value_type()),
        }
    }

    fn properties() -> &'static [ParamSpec] {
        static PROPERTIES: LazyLock<Vec<ParamSpec>> = LazyLock::new(|| {
            vec![
                glib::ParamSpecEnum::builder::<HashMethod>("hash-method")
                    .nick("Hash Method")
                    .blurb("Hash algorithm to use")
                    .mutable_ready()
                    .build(),
                ParamSpecString::builder("private-key-path")
                    .nick("Private Key Path")
                    .blurb("Path to PEM-encoded private key")
                    .mutable_ready()
                    .build(),
                ParamSpecString::builder("public-key-uri")
                    .nick("Public Key URI")
                    .blurb("URI of the public key for signature verification")
                    .mutable_ready()
                    .build(),
                ParamSpecString::builder("content-uuid")
                    .nick("Content UUID")
                    .blurb("Content UUID as hex string (32 characters)")
                    .mutable_ready()
                    .build(),
                ParamSpecUInt::builder("substream-length")
                    .nick("Substream Length")
                    .blurb("Number of buffers per substream (GOP length)")
                    .default_value(5)
                    .minimum(1)
                    .mutable_ready()
                    .build(),
            ]
        });
        PROPERTIES.as_ref()
    }
}

impl GstObjectImpl for DscSigner {}
impl ElementImpl for DscSigner {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "DSC Signer",
                "Generic",
                "Signs video buffers using H.274 DSC SEI metadata",
                "Diego Nieto <dnieto@fluendo.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = [
                gst::Structure::builder("video/x-h265")
                    .field("alignment", "au")
                    .field(
                        "stream-format",
                        gst::List::new(["byte-stream", "hvc1", "hev1"]),
                    )
                    .build(),
                gst::Structure::builder("video/x-h266")
                    .field("alignment", "au")
                    .field(
                        "stream-format",
                        gst::List::new(["byte-stream", "vvc1", "vvi1"]),
                    )
                    .build(),
            ]
            .into_iter()
            .collect::<gst::Caps>();
            vec![
                gst::PadTemplate::new(
                    "sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap(),
                gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap(),
            ]
        });
        TEMPLATES.as_ref()
    }
}

impl BaseTransformImpl for DscSigner {
    const MODE: gst_base::subclass::BaseTransformMode =
        gst_base::subclass::BaseTransformMode::AlwaysInPlace;
    const PASSTHROUGH_ON_SAME_CAPS: bool = false;
    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

    fn start(&self) -> Result<(), gst::ErrorMessage> {
        let private_key_path = self.settings.lock().unwrap().private_key_path.clone();

        *self.state.lock().unwrap() = State::default();

        if let Some(path) = private_key_path {
            let key_data = fs::read(&path).map_err(|e| {
                gst::error_msg!(
                    gst::ResourceError::NotFound,
                    ["Failed to read private key file {path}: {e}"]
                )
            })?;

            let key_pem = String::from_utf8(key_data).map_err(|e| {
                gst::error_msg!(
                    gst::CoreError::Failed,
                    ["Private key at {path} is not valid UTF-8 PEM data: {e}"]
                )
            })?;

            let pkey = RsaPrivateKey::from_pkcs8_pem(&key_pem)
                .or_else(|_| RsaPrivateKey::from_pkcs1_pem(&key_pem))
                .map_err(|e| {
                    gst::error_msg!(
                        gst::CoreError::Failed,
                        ["Invalid RSA private key at {path}: {e}"]
                    )
                })?;

            self.state.lock().unwrap().private_key = Some(pkey);
            gst::info!(CAT, "Loaded RSA private key from {path}");
        } else {
            gst::warning!(CAT, "No private-key-path configured at start");
        }

        Ok(())
    }

    fn stop(&self) -> Result<(), gst::ErrorMessage> {
        *self.state.lock().unwrap() = State::default();
        Ok(())
    }

    fn set_caps(&self, incaps: &gst::Caps, outcaps: &gst::Caps) -> Result<(), gst::LoggableError> {
        gst::debug!(CAT, imp = self, "Negotiating caps");
        gst::debug!(CAT, imp = self, "Input caps: {incaps}");
        gst::debug!(CAT, imp = self, "Output caps: {outcaps}");

        let (codec, stream_format) = VideoCodec::from_caps(incaps)
            .map_err(|e| gst::loggable_error!(CAT, "Unsupported caps {incaps}: {e}"))?;

        let mut state = self.state.lock().unwrap();
        state.gop_state.nal_parser = Some(NalParser::new(codec, stream_format));
        gst::info!(
            CAT,
            imp = self,
            "Initialized NAL parser for codec: {codec:?}, stream-format: {stream_format:?}",
        );

        Ok(())
    }

    fn sink_event(&self, event: gst::Event) -> bool {
        if let gst::EventView::Eos(_) = event.view() {
            let unsigned_buffers = {
                let state = self.state.lock().unwrap();
                if state.gop_state.gop_started {
                    state.gop_state.buffers_in_substream
                } else {
                    0
                }
            };

            if unsigned_buffers > 0 {
                gst::element_warning!(
                    self.obj(),
                    gst::CoreError::Failed,
                    ("EOS with {unsigned_buffers} buffer(s) in the final substream; the final substream is not signed")
                );
            }
        }

        self.parent_sink_event(event)
    }

    fn transform_ip(
        &self,
        buffer: &mut gst::BufferRef,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst::trace!(CAT, imp = self, "DscSigner transform_ip called");

        let obj = self.obj();
        let (hash_method, substream_length, content_uuid, public_key_uri) = {
            let settings = self.settings.lock().unwrap();
            (
                settings.hash_method,
                settings.substream_length,
                settings.content_uuid,
                settings.public_key_uri.clone(),
            )
        };

        let nal_parser = {
            let mut state = self.state.lock().unwrap();
            let gop_state = &mut state.gop_state;

            let is_first_buffer_in_substream = gop_state.buffers_in_substream == 0;

            if is_first_buffer_in_substream {
                gst::info!(
                    CAT,
                    imp = self,
                    "📝 Starting NEW substream (buffer 1/{substream_length})"
                );

                let hash_method_byte: u8 = hash_method.into();

                self.add_initialization_meta(
                    buffer,
                    hash_method_byte,
                    content_uuid.as_ref(),
                    public_key_uri.as_deref(),
                )?;

                let new_dsc_manager =
                    match DscSubstreamManager::new(hash_method, hash_method_byte, content_uuid) {
                        Ok(mut manager) => {
                            if let Some(ref last) = gop_state.last_digest {
                                manager.last_digest = Some(last.clone());
                            }
                            manager
                        }
                        Err(e) => {
                            gst::element_error!(
                                obj,
                                gst::CoreError::Failed,
                                ["Failed to create DscSubstreamManager: {e}"]
                            );
                            return Err(gst::FlowError::Error);
                        }
                    };

                gop_state.dsc_manager = Some(new_dsc_manager);
                gop_state.gop_started = true;
            }

            gop_state.nal_parser.clone()
        };

        self.add_selection_meta(buffer, 0)?;

        let map = buffer.map_readable().map_err(|_| {
            gst::error!(CAT, imp = self, "Failed to map buffer for reading");
            gst::FlowError::Error
        })?;

        let nal_units_to_hash = if let Some(ref nal_parser) = nal_parser {
            match nal_parser.extract_signable_data(&map) {
                Ok(nal_units) => {
                    gst::debug!(
                        CAT,
                        imp = self,
                        "Extracted {} NAL units from {} raw bytes",
                        nal_units.len(),
                        map.len()
                    );
                    nal_units
                }
                Err(e) => {
                    gst::element_error!(obj, gst::CoreError::Failed, ["NAL parsing failed: {e}"]);
                    return Err(gst::FlowError::Error);
                }
            }
        } else {
            gst::element_error!(
                obj,
                gst::CoreError::Negotiation,
                ["No NAL parser available, caps were not negotiated"]
            );
            return Err(gst::FlowError::NotNegotiated);
        };

        drop(map);

        let mut guard = self.state.lock().unwrap();
        let state = &mut *guard;

        if let Some(ref mut dsc_manager) = state.gop_state.dsc_manager {
            for nal_data in &nal_units_to_hash {
                if let Err(e) = dsc_manager.add_to_substream(0, nal_data) {
                    gst::error!(CAT, imp = self, "Failed to add NAL to substream: {}", e);
                    return Err(gst::FlowError::Error);
                }

                gst::trace!(
                    CAT,
                    imp = self,
                    "Added NAL to substream, size: {}",
                    nal_data.len()
                );
            }
        }

        state.gop_state.buffers_in_substream += 1;
        gst::debug!(
            CAT,
            imp = self,
            "Buffer {}/{} in current substream",
            state.gop_state.buffers_in_substream,
            substream_length
        );

        if state.gop_state.buffers_in_substream >= substream_length {
            gst::info!(
                CAT,
                imp = self,
                "🔐 LAST buffer in substream - creating signature"
            );

            if state.private_key.is_none() {
                state.private_key = Some(self.load_private_key_from_settings_for_runtime()?);
            }
            let private_key = state
                .private_key
                .as_ref()
                .expect("private key loaded above");
            let gop_state = &mut state.gop_state;

            if let Some(ref mut dsc_manager) = gop_state.dsc_manager {
                match dsc_manager.create_data_packet(0) {
                    Ok(data_packet) => {
                        gst::debug!(
                            CAT,
                            imp = self,
                            "Created data packet: {} bytes",
                            data_packet.len()
                        );

                        let signature = self.create_signature_from_data_packet(
                            &data_packet,
                            hash_method,
                            private_key,
                        )?;
                        gst::info!(
                            CAT,
                            imp = self,
                            "✅ Created signature ({} bytes) for substream",
                            signature.len()
                        );

                        self.add_verification_meta(buffer, &signature, 0)?;

                        gst::info!(
                            CAT,
                            imp = self,
                            "✅ Attached verification metadata to last buffer"
                        );
                    }
                    Err(e) => {
                        gst::error!(CAT, imp = self, "Failed to create data packet: {e}");
                        return Err(gst::FlowError::Error);
                    }
                }
            }

            let completed_last_digest = gop_state
                .dsc_manager
                .as_ref()
                .and_then(|m| m.last_digest.clone());

            gop_state.dsc_manager = None;
            gop_state.gop_started = false;
            gop_state.buffers_in_substream = 0;
            gop_state.last_digest = completed_last_digest;
        }

        Ok(gst::FlowSuccess::Ok)
    }
}

impl DscSigner {
    fn load_private_key_from_settings_for_runtime(&self) -> Result<RsaPrivateKey, gst::FlowError> {
        let path = match self.settings.lock().unwrap().private_key_path.clone() {
            Some(path) => path,
            None => {
                gst::error!(CAT, imp = self, "No private-key-path configured");
                return Err(gst::FlowError::Error);
            }
        };

        let key_data = fs::read(&path).map_err(|e| {
            gst::error!(
                CAT,
                imp = self,
                "Failed to read private key file {path}: {e}",
            );
            gst::FlowError::Error
        })?;

        let key_pem = String::from_utf8(key_data).map_err(|e| {
            gst::error!(
                CAT,
                imp = self,
                "Private key at {path} is not valid UTF-8 PEM data: {e}",
            );
            gst::FlowError::Error
        })?;

        let pkey = RsaPrivateKey::from_pkcs8_pem(&key_pem)
            .or_else(|_| RsaPrivateKey::from_pkcs1_pem(&key_pem))
            .map_err(|e| {
                gst::error!(CAT, imp = self, "Invalid RSA private key at {path}: {e}");
                gst::FlowError::Error
            })?;

        gst::info!(
            CAT,
            imp = self,
            "Loaded RSA private key from {path} (lazy runtime load)"
        );

        Ok(pkey)
    }

    fn add_initialization_meta(
        &self,
        buffer: &mut gst::BufferRef,
        hash_method_type: u8,
        content_uuid: Option<&[u8; 16]>,
        key_source_uri: Option<&str>,
    ) -> Result<(), gst::FlowError> {
        use gst_video::video_meta::VideoDSCInitializationMeta;

        if let Some(meta) = buffer.meta_mut::<gst_video::video_meta::VideoDSCInitializationMeta>() {
            gst::warning!(
                CAT,
                imp = self,
                "Stale initialization meta on input buffer — replacing"
            );
            let _ = meta.remove();
        }

        let dsc_init = gst_video::H274DigitallySignedContentInitialization::new(
            hash_method_type,
            content_uuid.copied(),
            key_source_uri,
        );

        VideoDSCInitializationMeta::add(buffer, &dsc_init);

        gst::info!(CAT, imp = self, "📝 Added DSC initialization metadata");

        Ok(())
    }

    fn add_selection_meta(
        &self,
        buffer: &mut gst::BufferRef,
        substream_id: u8,
    ) -> Result<(), gst::FlowError> {
        use gst_video::video_meta::VideoDSCSelectionMeta;

        if let Some(meta) = buffer.meta_mut::<gst_video::video_meta::VideoDSCSelectionMeta>() {
            gst::warning!(
                CAT,
                imp = self,
                "Stale selection meta on input buffer — replacing"
            );
            let _ = meta.remove();
        }

        let dsc_selection = gst_video::H274DigitallySignedContentSelection::new(substream_id);

        VideoDSCSelectionMeta::add(buffer, &dsc_selection);

        Ok(())
    }

    fn add_verification_meta(
        &self,
        buffer: &mut gst::BufferRef,
        signature: &[u8],
        substream_id: u8,
    ) -> Result<(), gst::FlowError> {
        use gst_video::video_meta::VideoDSCVerificationMeta;

        if let Some(meta) = buffer.meta_mut::<gst_video::video_meta::VideoDSCVerificationMeta>() {
            gst::warning!(
                CAT,
                imp = self,
                "Stale verification meta on input buffer — replacing"
            );
            let _ = meta.remove();
        }

        let dsc_verification =
            gst_video::H274DigitallySignedContentVerification::new(substream_id, signature);

        VideoDSCVerificationMeta::add(buffer, &dsc_verification);

        gst::info!(
            CAT,
            imp = self,
            "🔐 Added DSC verification metadata (signature: {} bytes)",
            signature.len()
        );

        Ok(())
    }

    fn create_signature_from_data_packet(
        &self,
        data_packet: &[u8],
        hash_method: HashMethod,
        pkey: &RsaPrivateKey,
    ) -> Result<Vec<u8>, gst::FlowError> {
        gst::debug!(
            CAT,
            imp = self,
            "SIGNER: Data packet first 32: {:02x?}",
            &data_packet[..std::cmp::min(32, data_packet.len())]
        );
        gst::debug!(
            CAT,
            imp = self,
            "SIGNER: Data packet last 32: {:02x?}",
            &data_packet[data_packet.len().saturating_sub(32)..]
        );

        let signature = self
            .create_rsa_pkcs1v15_signature(hash_method, pkey, data_packet)
            .map_err(|e| {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to create RSA PKCS#1 v1.5 signature with {:?}: {}",
                    hash_method,
                    e
                );
                gst::FlowError::Error
            })?;

        gst::debug!(
            CAT,
            imp = self,
            "Created signature: {} bytes",
            signature.len()
        );
        Ok(signature)
    }

    fn create_rsa_pkcs1v15_signature(
        &self,
        hash_method: HashMethod,
        pkey: &RsaPrivateKey,
        data_packet: &[u8],
    ) -> std::result::Result<Vec<u8>, String> {
        match hash_method {
            HashMethod::Sha1 => {
                let signing_key = SigningKey::<Sha1>::new(pkey.clone());
                Ok(signing_key.sign(data_packet).to_vec())
            }
            HashMethod::Sha224 => {
                let signing_key = SigningKey::<Sha224>::new(pkey.clone());
                Ok(signing_key.sign(data_packet).to_vec())
            }
            HashMethod::Sha256 => {
                let signing_key = SigningKey::<Sha256>::new(pkey.clone());
                Ok(signing_key.sign(data_packet).to_vec())
            }
            HashMethod::Sha384 => {
                let signing_key = SigningKey::<Sha384>::new(pkey.clone());
                Ok(signing_key.sign(data_packet).to_vec())
            }
            HashMethod::Sha512 => {
                let signing_key = SigningKey::<Sha512>::new(pkey.clone());
                Ok(signing_key.sign(data_packet).to_vec())
            }
        }
    }
}
