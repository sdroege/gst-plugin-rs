// encrypter.rs
//
// Copyright 2019 Jordan Petridis <jordan@centricular.com>
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to
// deal in the Software without restriction, including without limitation the
// rights to use, copy, modify, merge, publish, distribute, sublicense, and/or
// sell copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS
// IN THE SOFTWARE.
//
// SPDX-License-Identifier: MIT

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;
use gst::{gst_debug, gst_error, gst_log};
use smallvec::SmallVec;
use sodiumoxide::crypto::box_;

type BufferVec = SmallVec<[gst::Buffer; 16]>;

use std::sync::Mutex;

use once_cell::sync::Lazy;
static CAT: Lazy<gst::DebugCategory> = Lazy::new(|| {
    gst::DebugCategory::new(
        "sodiumencrypter",
        gst::DebugColorFlags::empty(),
        Some("Encrypter Element"),
    )
});

#[derive(Debug, Clone)]
struct Props {
    receiver_key: Option<glib::Bytes>,
    sender_key: Option<glib::Bytes>,
    block_size: u32,
}

impl Default for Props {
    fn default() -> Self {
        Props {
            receiver_key: None,
            sender_key: None,
            block_size: 32768,
        }
    }
}

#[derive(Debug)]
struct State {
    adapter: gst_base::UniqueAdapter,
    nonce: box_::Nonce,
    precomputed_key: box_::PrecomputedKey,
    block_size: u32,
    write_headers: bool,
}

impl State {
    fn from_props(props: &Props) -> Result<Self, gst::ErrorMessage> {
        let sender_key = props
            .sender_key
            .as_ref()
            .and_then(|k| box_::SecretKey::from_slice(&k))
            .ok_or_else(|| {
                gst::error_msg!(
                    gst::ResourceError::NotFound,
                    [format!(
                        "Failed to set Sender's Key from property: {:?}",
                        props.sender_key
                    )
                    .as_ref()]
                )
            })?;

        let receiver_key = props
            .receiver_key
            .as_ref()
            .and_then(|k| box_::PublicKey::from_slice(&k))
            .ok_or_else(|| {
                gst::error_msg!(
                    gst::ResourceError::NotFound,
                    [format!(
                        "Failed to set Receiver's Key from property: {:?}",
                        props.receiver_key
                    )
                    .as_ref()]
                )
            })?;

        // This env variable is only meant to bypass nonce regeneration during
        // tests to get determinisic results. It should never be used outside
        // of testing environments.
        let nonce = if let Ok(val) = std::env::var("GST_SODIUM_ENCRYPT_NONCE") {
            let bytes = hex::decode(val).expect("Failed to decode hex variable");
            assert_eq!(bytes.len(), box_::NONCEBYTES);
            box_::Nonce::from_slice(&bytes).unwrap()
        } else {
            box_::gen_nonce()
        };

        let precomputed_key = box_::precompute(&receiver_key, &sender_key);

        Ok(Self {
            adapter: gst_base::UniqueAdapter::new(),
            precomputed_key,
            nonce,
            block_size: props.block_size,
            write_headers: true,
        })
    }

    fn seal(&mut self, message: &[u8]) -> Vec<u8> {
        let ciphertext = box_::seal_precomputed(message, &self.nonce, &self.precomputed_key);
        self.nonce.increment_le_inplace();
        ciphertext
    }

    fn encrypt_message(&mut self, buffer: &gst::BufferRef) -> gst::Buffer {
        let map = buffer
            .map_readable()
            .expect("Failed to map buffer readable");

        let sealed = self.seal(&map);
        gst::Buffer::from_mut_slice(sealed)
    }

    fn encrypt_blocks(&mut self, block_size: usize) -> BufferVec {
        assert_ne!(block_size, 0);

        let mut buffers = BufferVec::new();

        // As long we have enough bytes to encrypt a block, or more, we do so
        // else the leftover bytes on the adapter will be pushed when EOS
        // is sent.
        while self.adapter.available() >= block_size {
            let buffer = self.adapter.take_buffer(block_size).unwrap();
            let out_buf = self.encrypt_message(&buffer);

            buffers.push(out_buf);
        }

        buffers
    }
}

pub struct Encrypter {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    props: Mutex<Props>,
    state: Mutex<Option<State>>,
}

impl Encrypter {
    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &super::Encrypter,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        gst_log!(CAT, obj: pad, "Handling buffer {:?}", buffer);

        let mut buffers = BufferVec::new();
        let mut state_guard = self.state.lock().unwrap();
        let state = state_guard.as_mut().unwrap();

        if state.write_headers {
            let mut headers = Vec::with_capacity(40);
            headers.extend_from_slice(crate::TYPEFIND_HEADER);
            // Write the Nonce used into the stream.
            headers.extend_from_slice(state.nonce.as_ref());
            // Write the block_size into the stream
            headers.extend_from_slice(&state.block_size.to_le_bytes());

            buffers.push(gst::Buffer::from_mut_slice(headers));
            state.write_headers = false;
        }

        state.adapter.push(buffer);

        // Encrypt the whole blocks, if any, and push them.
        buffers.extend(state.encrypt_blocks(state.block_size as usize));

        drop(state_guard);

        for buffer in buffers {
            self.srcpad.push(buffer).map_err(|err| {
                gst_error!(CAT, obj: element, "Failed to push buffer {:?}", err);
                err
            })?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, pad: &gst::Pad, element: &super::Encrypter, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Caps(_) => {
                // We send our own caps downstream
                let caps = gst::Caps::builder("application/x-sodium-encrypted").build();
                self.srcpad.push_event(gst::event::Caps::new(&caps))
            }
            EventView::Eos(_) => {
                let mut state_mutex = self.state.lock().unwrap();
                let mut buffers = BufferVec::new();
                // This will only be run after READY state,
                // and will be guaranted to be initialized
                let state = state_mutex.as_mut().unwrap();

                // Now that all the full size blocks are pushed, drain the
                // rest of the adapter and push whatever is left.
                let avail = state.adapter.available();
                // logic error, all the complete blocks that can be pushed
                // should have been done in the sink_chain call.
                assert!(avail < state.block_size as usize);

                if avail > 0 {
                    let b = state.encrypt_blocks(avail);
                    buffers.extend(b);
                }

                // drop the lock before pushing into the pad
                drop(state_mutex);

                for buffer in buffers {
                    if let Err(err) = self.srcpad.push(buffer) {
                        gst_error!(CAT, obj: element, "Failed to push buffer at EOS {:?}", err);
                        return false;
                    }
                }

                pad.event_default(Some(element), event)
            }
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &super::Encrypter, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Seek(_) => false,
            _ => pad.event_default(Some(element), event),
        }
    }

    fn src_query(
        &self,
        pad: &gst::Pad,
        element: &super::Encrypter,
        query: &mut gst::QueryRef,
    ) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Seeking(mut q) => {
                let format = q.format();
                q.set(
                    false,
                    gst::GenericFormattedValue::Other(format, -1),
                    gst::GenericFormattedValue::Other(format, -1),
                );
                gst_log!(CAT, obj: pad, "Returning {:?}", q.query_mut());
                true
            }
            QueryView::Duration(ref mut q) => {
                use std::convert::TryInto;

                if q.format() != gst::Format::Bytes {
                    return pad.query_default(Some(element), query);
                }

                /* First let's query the bytes duration upstream */
                let mut peer_query = gst::query::Duration::new(gst::Format::Bytes);

                if !self.sinkpad.peer_query(&mut peer_query) {
                    gst_error!(CAT, "Failed to query upstream duration");
                    return false;
                }

                let size = match peer_query.result().try_into().unwrap() {
                    Some(gst::format::Bytes(size)) => size,
                    None => {
                        gst_error!(CAT, "Failed to query upstream duration");

                        return false;
                    }
                };

                let state = self.state.lock().unwrap();
                let state = match state.as_ref() {
                    // If state isn't set, it means that the
                    // element hasn't been activated yet.
                    None => return false,
                    Some(s) => s,
                };

                // calculate the number of chunks that exist in the stream
                let total_chunks = (size + state.block_size as u64 - 1) / state.block_size as u64;
                // add the MAC of each block
                let size = size + total_chunks * box_::MACBYTES as u64;

                // add static offsets
                let size = size + crate::HEADERS_SIZE as u64;

                gst_debug!(CAT, obj: pad, "Setting duration bytes: {}", size);
                q.set(gst::format::Bytes(size));

                true
            }
            _ => pad.query_default(Some(element), query),
        }
    }
}

#[glib::object_subclass]
impl ObjectSubclass for Encrypter {
    const NAME: &'static str = "RsSodiumEncrypter";
    type Type = super::Encrypter;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_with_template(&templ, Some("sink"))
            .chain_function(|pad, parent, buffer| {
                Encrypter::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |encrypter, element| encrypter.sink_chain(pad, element, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Encrypter::catch_panic_pad_function(
                    parent,
                    || false,
                    |encrypter, element| encrypter.sink_event(pad, element, event),
                )
            })
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_with_template(&templ, Some("src"))
            .query_function(|pad, parent, query| {
                Encrypter::catch_panic_pad_function(
                    parent,
                    || false,
                    |encrypter, element| encrypter.src_query(pad, element, query),
                )
            })
            .event_function(|pad, parent, event| {
                Encrypter::catch_panic_pad_function(
                    parent,
                    || false,
                    |encrypter, element| encrypter.src_event(pad, element, event),
                )
            })
            .build();

        let props = Mutex::new(Props::default());
        let state = Mutex::new(None);

        Self {
            srcpad,
            sinkpad,
            props,
            state,
        }
    }
}

impl ObjectImpl for Encrypter {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: Lazy<Vec<glib::ParamSpec>> = Lazy::new(|| {
            vec![
                glib::ParamSpec::new_boxed(
                    "receiver-key",
                    "Receiver Key",
                    "The public key of the Receiver",
                    glib::Bytes::static_type(),
                    glib::ParamFlags::READWRITE,
                ),
                glib::ParamSpec::new_boxed(
                    "sender-key",
                    "Sender Key",
                    "The private key of the Sender",
                    glib::Bytes::static_type(),
                    glib::ParamFlags::WRITABLE,
                ),
                glib::ParamSpec::new_uint(
                    "block-size",
                    "Block Size",
                    "The block-size of the chunks",
                    1024,
                    std::u32::MAX,
                    32768,
                    glib::ParamFlags::READWRITE,
                ),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self, obj: &Self::Type) {
        self.parent_constructed(obj);

        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(
        &self,
        _obj: &Self::Type,
        _id: usize,
        value: &glib::Value,
        pspec: &glib::ParamSpec,
    ) {
        match pspec.name() {
            "sender-key" => {
                let mut props = self.props.lock().unwrap();
                props.sender_key = value.get().expect("type checked upstream");
            }

            "receiver-key" => {
                let mut props = self.props.lock().unwrap();
                props.receiver_key = value.get().expect("type checked upstream");
            }

            "block-size" => {
                let mut props = self.props.lock().unwrap();
                props.block_size = value.get().expect("type checked upstream");
            }

            _ => unimplemented!(),
        }
    }

    fn property(&self, _obj: &Self::Type, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "receiver-key" => {
                let props = self.props.lock().unwrap();
                props.receiver_key.to_value()
            }

            "block-size" => {
                let props = self.props.lock().unwrap();
                props.block_size.to_value()
            }

            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for Encrypter {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: Lazy<gst::subclass::ElementMetadata> = Lazy::new(|| {
            gst::subclass::ElementMetadata::new(
                "Encrypter",
                "Generic",
                "libsodium-based file encrypter",
                "Jordan Petridis <jordan@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: Lazy<Vec<gst::PadTemplate>> = Lazy::new(|| {
            let src_caps = gst::Caps::builder("application/x-sodium-encrypted").build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &src_caps,
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &gst::Caps::new_any(),
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }

    fn change_state(
        &self,
        element: &Self::Type,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_debug!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                let props = self.props.lock().unwrap().clone();

                // Create an internal state struct from the provided properties or
                // refuse to change state
                let state_ = State::from_props(&props).map_err(|err| {
                    element.post_error_message(err);
                    gst::StateChangeError
                })?;

                let mut state = self.state.lock().unwrap();
                *state = Some(state_);
            }
            gst::StateChange::ReadyToNull => {
                let _ = self.state.lock().unwrap().take();
            }
            _ => (),
        }

        let success = self.parent_change_state(element, transition)?;

        if transition == gst::StateChange::ReadyToNull {
            let _ = self.state.lock().unwrap().take();
        }

        Ok(success)
    }
}
