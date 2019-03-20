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

use glib::prelude::*;
use glib::subclass;
use glib::subclass::prelude::*;
use gst::prelude::*;
use gst::subclass::prelude::*;
use smallvec::SmallVec;
use sodiumoxide::crypto::box_;

type BufferVec = SmallVec<[gst::Buffer; 16]>;

use std::sync::Mutex;

lazy_static! {
    static ref CAT: gst::DebugCategory = {
        gst::DebugCategory::new(
            "rssodiumencrypter",
            gst::DebugColorFlags::empty(),
            "Encrypter Element",
        )
    };
}

static PROPERTIES: [subclass::Property; 3] = [
    subclass::Property("receiver-key", |name| {
        glib::ParamSpec::boxed(
            name,
            "Receiver Key",
            "The public key of the Receiver",
            glib::Bytes::static_type(),
            glib::ParamFlags::READWRITE,
        )
    }),
    subclass::Property("sender-key", |name| {
        glib::ParamSpec::boxed(
            name,
            "Sender Key",
            "The private key of the Sender",
            glib::Bytes::static_type(),
            glib::ParamFlags::WRITABLE,
        )
    }),
    subclass::Property("block-size", |name| {
        glib::ParamSpec::uint(
            name,
            "Block Size",
            "The block-size of the chunks",
            1024,
            std::u32::MAX,
            32768,
            glib::ParamFlags::READWRITE,
        )
    }),
];

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
                gst_error_msg!(
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
                gst_error_msg!(
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

    fn encrypt_blocks(&mut self, block_size: usize) -> Result<BufferVec, gst::FlowError> {
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

        Ok(buffers)
    }
}

struct Encrypter {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    props: Mutex<Props>,
    state: Mutex<Option<State>>,
}

impl Encrypter {
    fn set_pad_functions(sinkpad: &gst::Pad, srcpad: &gst::Pad) {
        sinkpad.set_chain_function(|pad, parent, buffer| {
            Encrypter::catch_panic_pad_function(
                parent,
                || Err(gst::FlowError::Error),
                |encrypter, element| encrypter.sink_chain(pad, element, buffer),
            )
        });
        sinkpad.set_event_function(|pad, parent, event| {
            Encrypter::catch_panic_pad_function(
                parent,
                || false,
                |encrypter, element| encrypter.sink_event(pad, element, event),
            )
        });

        srcpad.set_query_function(|pad, parent, query| {
            Encrypter::catch_panic_pad_function(
                parent,
                || false,
                |encrypter, element| encrypter.src_query(pad, element, query),
            )
        });
        srcpad.set_event_function(|pad, parent, event| {
            Encrypter::catch_panic_pad_function(
                parent,
                || false,
                |encrypter, element| encrypter.src_event(pad, element, event),
            )
        });
    }

    fn sink_chain(
        &self,
        pad: &gst::Pad,
        element: &gst::Element,
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
        buffers.extend(
            state
                .encrypt_blocks(state.block_size as usize)
                .map_err(|err| {
                    // log the error to the bus
                    gst_element_error!(
                        element,
                        gst::ResourceError::Write,
                        ["Failed to decrypt buffer"]
                    );
                    err
                })?,
        );

        drop(state);
        drop(state_guard);

        for buffer in buffers {
            self.srcpad.push(buffer).map_err(|err| {
                gst_error!(CAT, obj: element, "Failed to push buffer {:?}", err);
                err
            })?;
        }

        Ok(gst::FlowSuccess::Ok)
    }

    fn sink_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Caps(_) => {
                // We send our own caps downstream
                let caps = gst::Caps::builder("application/x-sodium-encrypted").build();
                self.srcpad.push_event(gst::Event::new_caps(&caps).build())
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
                    match state.encrypt_blocks(avail) {
                        Err(_) => {
                            gst_element_error!(
                                element,
                                gst::ResourceError::Write,
                                ["Failed to encrypt buffers at EOS"]
                            );
                            return false;
                        }
                        Ok(b) => buffers.extend(b),
                    }
                }

                // drop the lock before pushing into the pad
                drop(state);
                drop(state_mutex);

                for buffer in buffers {
                    if let Err(err) = self.srcpad.push(buffer) {
                        gst_error!(CAT, obj: element, "Failed to push buffer at EOS {:?}", err);
                        return false;
                    }
                }

                pad.event_default(element, event)
            }
            _ => pad.event_default(element, event),
        }
    }

    fn src_event(&self, pad: &gst::Pad, element: &gst::Element, event: gst::Event) -> bool {
        use gst::EventView;

        gst_log!(CAT, obj: pad, "Handling event {:?}", event);

        match event.view() {
            EventView::Seek(_) => false,
            _ => pad.event_default(element, event),
        }
    }

    fn src_query(&self, pad: &gst::Pad, element: &gst::Element, query: &mut gst::QueryRef) -> bool {
        use gst::QueryView;

        gst_log!(CAT, obj: pad, "Handling query {:?}", query);

        match query.view_mut() {
            QueryView::Seeking(mut q) => {
                let format = q.get_format();
                q.set(
                    false,
                    gst::GenericFormattedValue::Other(format, -1),
                    gst::GenericFormattedValue::Other(format, -1),
                );
                gst_log!(CAT, obj: pad, "Returning {:?}", q.get_mut_query());
                true
            }
            QueryView::Duration(ref mut q) => {
                if q.get_format() != gst::Format::Bytes {
                    return pad.query_default(element, query);
                }

                /* First let's query the bytes duration upstream */
                let mut peer_query = gst::query::Query::new_duration(gst::Format::Bytes);

                if !self.sinkpad.peer_query(&mut peer_query) {
                    gst_error!(CAT, "Failed to query upstream duration");
                    return false;
                }

                let size = match peer_query.get_result().try_into_bytes().unwrap() {
                    gst::format::Bytes(Some(size)) => size,
                    gst::format::Bytes(None) => {
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
                let size = size + super::HEADERS_SIZE as u64;

                gst_debug!(CAT, obj: pad, "Setting duration bytes: {}", size);
                q.set(gst::format::Bytes::from(size));

                true
            }
            _ => pad.query_default(element, query),
        }
    }
}

impl ObjectSubclass for Encrypter {
    const NAME: &'static str = "RsSodiumEncrypter";
    type ParentType = gst::Element;
    type Instance = gst::subclass::ElementInstanceStruct<Self>;
    type Class = subclass::simple::ClassStruct<Self>;

    glib_object_subclass!();

    fn new_with_class(klass: &subclass::simple::ClassStruct<Self>) -> Self {
        let templ = klass.get_pad_template("sink").unwrap();
        let sinkpad = gst::Pad::new_from_template(&templ, Some("sink"));
        let templ = klass.get_pad_template("src").unwrap();
        let srcpad = gst::Pad::new_from_template(&templ, Some("src"));

        Encrypter::set_pad_functions(&sinkpad, &srcpad);
        let props = Mutex::new(Props::default());
        let state = Mutex::new(None);

        Self {
            srcpad,
            sinkpad,
            props,
            state,
        }
    }

    fn class_init(klass: &mut subclass::simple::ClassStruct<Self>) {
        klass.set_metadata(
            "Encrypter",
            "Generic",
            "libsodium-based file encrypter",
            "Jordan Petridis <jordan@centricular.com>",
        );

        let src_caps = gst::Caps::builder("application/x-sodium-encrypted").build();
        let src_pad_template = gst::PadTemplate::new(
            "src",
            gst::PadDirection::Src,
            gst::PadPresence::Always,
            &src_caps,
        )
        .unwrap();
        klass.add_pad_template(src_pad_template);

        let sink_pad_template = gst::PadTemplate::new(
            "sink",
            gst::PadDirection::Sink,
            gst::PadPresence::Always,
            &gst::Caps::new_any(),
        )
        .unwrap();
        klass.add_pad_template(sink_pad_template);
        klass.install_properties(&PROPERTIES);
    }
}

impl ObjectImpl for Encrypter {
    glib_object_impl!();

    fn constructed(&self, obj: &glib::Object) {
        self.parent_constructed(obj);

        let element = obj.downcast_ref::<gst::Element>().unwrap();
        element.add_pad(&self.sinkpad).unwrap();
        element.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(&self, _obj: &glib::Object, id: usize, value: &glib::Value) {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("sender-key", ..) => {
                let mut props = self.props.lock().unwrap();
                props.sender_key = value.get();
            }

            subclass::Property("receiver-key", ..) => {
                let mut props = self.props.lock().unwrap();
                props.receiver_key = value.get();
            }

            subclass::Property("block-size", ..) => {
                let mut props = self.props.lock().unwrap();
                props.block_size = value.get().unwrap();
            }

            _ => unimplemented!(),
        }
    }

    fn get_property(&self, _obj: &glib::Object, id: usize) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id];

        match *prop {
            subclass::Property("receiver-key", ..) => {
                let props = self.props.lock().unwrap();
                Ok(props.receiver_key.to_value())
            }

            subclass::Property("block-size", ..) => {
                let props = self.props.lock().unwrap();
                Ok(props.block_size.to_value())
            }

            _ => unimplemented!(),
        }
    }
}

impl ElementImpl for Encrypter {
    fn change_state(
        &self,
        element: &gst::Element,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        gst_debug!(CAT, obj: element, "Changing state {:?}", transition);

        match transition {
            gst::StateChange::NullToReady => {
                let props = self.props.lock().unwrap().clone();

                // Create an internal state struct from the provided properties or
                // refuse to change state
                let state_ = State::from_props(&props).map_err(|err| {
                    element.post_error_message(&err);
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

pub fn register(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    gst::Element::register(Some(plugin), "rssodiumencrypter", 0, Encrypter::get_type())
}
