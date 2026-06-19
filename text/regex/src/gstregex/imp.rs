// Copyright (C) 2021 Mathieu Duponchelle <mathieu@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use super::RegExMultiBufferMode;

use gst::glib;
use gst::prelude::*;
use gst::subclass::prelude::*;

use regex::{Regex, RegexBuilder};
use std::default::Default;
use std::sync::Mutex;

use regex_automata::{
    HalfMatch, Input, MatchError,
    hybrid::dfa::{Cache, DFA},
    util::syntax,
};

#[derive(Debug)]
enum Prefix {
    /// We matched after signaling EOI, and were in the given
    /// state immediately beforehand.
    ///
    /// We may still match given more input.
    MatchAfterEoi,
    /// We encountered a dead or quit state before EOI, but
    /// matched at some point before that (guess you're not anchored).
    //
    // We won't match given more input.
    MatchOnlyBeforeEoi(HalfMatch),
    /// We did not match after EOI and were in the given
    /// non-dead, non-quit state immediately before EOI.
    ///
    /// I.e. we may still match given more input.
    ///
    /// The last match seen (if any) could be anywhere prior
    /// to EOI; it may or may not correspond to the state id.
    AtEoi,
    /// We never saw a match and encountered a `dead` state,
    /// i.e., we will never match
    NoMatch,
}

// Based on https://play.rust-lang.org/?version=stable&mode=debug&edition=2024&gist=9a0cb40a6120638fbb7e98f4c707b0a4
fn find_prefix(dfa: &DFA, cache: &mut Cache, haystack: &[u8]) -> Result<Prefix, MatchError> {
    let mut sid = dfa.start_state_forward(cache, &Input::new(haystack))?;
    let mut last_match = None;
    for (i, &b) in haystack.iter().enumerate() {
        sid = dfa
            .next_state(cache, sid, b)
            .map_err(|_| MatchError::gave_up(i))?;
        if sid.is_tagged() {
            if sid.is_match() {
                last_match = Some(HalfMatch::new(dfa.match_pattern(cache, sid, 0), i));
            } else if sid.is_dead() {
                return match last_match {
                    Some(lm) => Ok(Prefix::MatchOnlyBeforeEoi(lm)),
                    None => Ok(Prefix::NoMatch),
                };
            } else if sid.is_quit() {
                return match last_match {
                    Some(lm) => Ok(Prefix::MatchOnlyBeforeEoi(lm)),
                    None => Err(MatchError::quit(b, i)),
                };
            }
        }
    }
    let last_sid = dfa
        .next_eoi_state(cache, sid)
        .map_err(|_| MatchError::gave_up(haystack.len()))?;

    if last_sid.is_match() {
        Ok(Prefix::MatchAfterEoi)
    } else {
        Ok(Prefix::AtEoi)
    }
}

use std::sync::LazyLock;

static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
    gst::DebugCategory::new(
        "regex",
        gst::DebugColorFlags::empty(),
        Some("Regular Expression element"),
    )
});

enum Operation {
    ReplaceAll(String),
}

struct Command {
    pattern: String,
    regex: Regex,
    dfa: DFA,
    dfa_cache: Cache,
    operation: Operation,
}

#[derive(Debug)]
struct Accumulator {
    text: String,
    buffers: Vec<gst::Buffer>,
}

#[derive(Default)]
struct State {
    settings: Settings,
    commands: Vec<Command>,
    accumulator: Option<Accumulator>,
}

#[derive(Debug, Clone)]
pub(super) struct Settings {
    multi_buffer_mode: RegExMultiBufferMode,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            multi_buffer_mode: RegExMultiBufferMode::Disabled,
        }
    }
}

pub struct RegEx {
    srcpad: gst::Pad,
    sinkpad: gst::Pad,
    state: Mutex<State>,
    settings: Mutex<Settings>,
}

impl RegEx {
    fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
        gst::trace!(CAT, obj = pad, "Handling event {event:?}");

        use gst::EventView::*;
        match event.view() {
            FlushStart(_) => {
                let _ = self.state.lock().unwrap().accumulator.take();
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            Segment(_) => {
                let _ = self.sink_chain_internal(None);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            Eos(_) => {
                let _ = self.sink_chain_internal(None);
                gst::Pad::event_default(pad, Some(&*self.obj()), event)
            }
            _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
        }
    }

    fn sink_chain_internal(
        &self,
        buffer: Option<gst::Buffer>,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        let data = match buffer.as_ref() {
            Some(buffer) => {
                let data = buffer.map_readable().map_err(|_| {
                    gst::error!(CAT, imp = self, "Can't map buffer readable");
                    gst::element_imp_error!(self, gst::CoreError::Failed, ["Failed to map buffer"]);
                    gst::FlowError::Error
                })?;

                Some(
                    std::str::from_utf8(&data)
                        .map_err(|err| {
                            gst::error!(CAT, imp = self, "Can't decode utf8: {}", err);
                            gst::element_imp_error!(
                                self,
                                gst::StreamError::Decode,
                                ["Failed to decode utf8: {}", err]
                            );

                            gst::FlowError::Error
                        })?
                        .to_string(),
                )
            }
            None => None,
        };

        let mut state = self.state.lock().unwrap();

        let outbuf = match state.settings.multi_buffer_mode {
            RegExMultiBufferMode::Compress => {
                let mut forward = true;

                let accumulator = state.accumulator.get_or_insert_with(|| Accumulator {
                    text: "".to_string(),
                    buffers: vec![],
                });

                if let Some(data) = data {
                    accumulator.text.push_str(&data);
                    accumulator.text.push(' ');

                    accumulator.buffers.push(buffer.as_ref().unwrap().clone());
                } else if accumulator.text.is_empty() {
                    return Ok(gst::FlowSuccess::Ok);
                }

                let mut data = accumulator.text.clone();

                'outer: for command in state.commands.iter_mut() {
                    let mut dfa_data = data.clone();
                    'inner: loop {
                        let Ok(match_result) = find_prefix(
                            &command.dfa,
                            &mut command.dfa_cache,
                            dfa_data.to_string().as_bytes(),
                        ) else {
                            gst::error!(
                                CAT,
                                "find_prefix failed, please report an issue (haystack: {})",
                                dfa_data
                            );
                            gst::element_imp_error!(
                                self,
                                gst::CoreError::Failed,
                                [
                                    "find_prefix failed, please report an issue (haystack: {})",
                                    dfa_data
                                ]
                            );
                            return Err(gst::FlowError::Error);
                        };

                        match match_result {
                            Prefix::MatchAfterEoi => {
                                forward = false || buffer.is_none();
                                break 'outer;
                            }
                            Prefix::MatchOnlyBeforeEoi(half_match) => {
                                dfa_data = dfa_data.split_off(half_match.offset());
                            }
                            Prefix::NoMatch => {
                                break 'inner;
                            }
                            Prefix::AtEoi => {
                                forward = false || buffer.is_none();
                                break 'outer;
                            }
                        }
                    }

                    match &command.operation {
                        Operation::ReplaceAll(replacement) => {
                            data = command
                                .regex
                                .replace_all(&data, replacement.as_str())
                                .to_string();
                        }
                    }
                }

                if !forward {
                    gst::log!(
                        CAT,
                        "Got partial match in accumulator {:?}, not forwarding",
                        state.accumulator
                    );
                    return Ok(gst::FlowSuccess::Ok);
                } else {
                    let accumulator = state.accumulator.take().unwrap();

                    data.pop();

                    gst::debug!(CAT, "Forwarding contents of accumulator {:?}", accumulator);

                    let mut outbuf = gst::Buffer::from_mut_slice(data.into_bytes());

                    {
                        let outbuf_mut = outbuf.get_mut().unwrap();
                        for buffer in accumulator.buffers {
                            let _ = buffer.copy_into(
                                outbuf_mut,
                                gst::BufferCopyFlags::FLAGS
                                    | gst::BufferCopyFlags::TIMESTAMPS
                                    | gst::BufferCopyFlags::META,
                                ..,
                            );
                        }
                    }

                    outbuf
                }
            }
            RegExMultiBufferMode::Disabled => {
                let Some(mut data) = data else {
                    return Ok(gst::FlowSuccess::Ok);
                };

                for command in state.commands.iter_mut() {
                    match &command.operation {
                        Operation::ReplaceAll(replacement) => {
                            data = command
                                .regex
                                .replace_all(&data, replacement.as_str())
                                .to_string();
                        }
                    }
                }

                let mut outbuf = gst::Buffer::from_mut_slice(data.into_bytes());

                {
                    let outbuf_mut = outbuf.get_mut().unwrap();
                    let _ = buffer.unwrap().copy_into(
                        outbuf_mut,
                        gst::BufferCopyFlags::FLAGS
                            | gst::BufferCopyFlags::TIMESTAMPS
                            | gst::BufferCopyFlags::META,
                        ..,
                    );
                }

                outbuf
            }
        };

        drop(state);

        self.srcpad.push(outbuf)
    }

    fn sink_chain(
        &self,
        _pad: &gst::Pad,
        buffer: gst::Buffer,
    ) -> Result<gst::FlowSuccess, gst::FlowError> {
        self.sink_chain_internal(Some(buffer))
    }
}

#[glib::object_subclass]
impl ObjectSubclass for RegEx {
    const NAME: &'static str = "GstRegEx";
    type Type = super::RegEx;
    type ParentType = gst::Element;

    fn with_class(klass: &Self::Class) -> Self {
        let templ = klass.pad_template("sink").unwrap();
        let sinkpad = gst::Pad::builder_from_template(&templ)
            .chain_function(|pad, parent, buffer| {
                RegEx::catch_panic_pad_function(
                    parent,
                    || Err(gst::FlowError::Error),
                    |regex| regex.sink_chain(pad, buffer),
                )
            })
            .event_function(|pad, parent, event| {
                Self::catch_panic_pad_function(parent, || false, |imp| imp.sink_event(pad, event))
            })
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        let templ = klass.pad_template("src").unwrap();
        let srcpad = gst::Pad::builder_from_template(&templ)
            .flags(gst::PadFlags::PROXY_CAPS | gst::PadFlags::FIXED_CAPS)
            .build();

        let state = Mutex::new(State::default());
        let settings = Mutex::new(Settings::default());

        Self {
            srcpad,
            sinkpad,
            state,
            settings,
        }
    }
}

impl ObjectImpl for RegEx {
    fn properties() -> &'static [glib::ParamSpec] {
        static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
            vec![
                gst::ParamSpecArray::builder("commands")
                    .nick("Commands")
                    .blurb("A set of commands to apply on input text")
                    .element_spec(
                        &glib::ParamSpecBoxed::builder::<gst::Structure>("command")
                            .nick("Command")
                            .blurb("A command to apply on input text")
                            .build(),
                    )
                    .mutable_playing()
                    .build(),
                /**
                 * GstRegEx:multi-buffer-mode:
                 *
                 * How to accumulate input when it partially matches commands.
                 *
                 * When the element processes single words in its input buffers,
                 * it cannot match multi word patterns when multi-buffer-mode is disabled.
                 *
                 * When it is set to compress however, it will detect whether the word is a
                 * partial match for one of the commands, and accumulate it into a (space-separated)
                 * temporary buffer. Once none of the commands is a partial match anymore, the
                 * temporary buffer is output, with its timestamp / duration set to that of the last
                 * processed input buffer.
                 *
                 * Since: plugins-rs-0.16.0
                 */
                glib::ParamSpecEnum::builder_with_default(
                    "multi-buffer-mode",
                    Settings::default().multi_buffer_mode,
                )
                .nick("Multi Buffer Mode")
                .blurb("How to accumulate input when it partially matches commands")
                .build(),
            ]
        });

        PROPERTIES.as_ref()
    }

    fn constructed(&self) {
        self.parent_constructed();

        let obj = self.obj();
        obj.add_pad(&self.sinkpad).unwrap();
        obj.add_pad(&self.srcpad).unwrap();
    }

    fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
        match pspec.name() {
            "commands" => {
                let mut state = self.state.lock().unwrap();
                state.commands = vec![];
                let commands = value.get::<gst::ArrayRef>().expect("type checked upstream");
                for command in commands.as_slice() {
                    let s = match command
                        .get::<Option<gst::Structure>>()
                        .expect("type checked upstream")
                    {
                        Some(s) => s,
                        None => {
                            continue;
                        }
                    };
                    let operation = s.name();

                    let pattern = match s.get::<Option<String>>("pattern") {
                        Ok(Some(pattern)) => pattern,
                        Ok(None) | Err(_) => {
                            gst::error!(
                                CAT,
                                imp = self,
                                "All commands require a pattern field as a string"
                            );
                            continue;
                        }
                    };

                    let mut builder = RegexBuilder::new(&pattern);
                    builder
                        .unicode(s.get::<bool>("unicode").unwrap_or(true))
                        .case_insensitive(s.get::<bool>("case-insensitive").unwrap_or(false))
                        .multi_line(s.get::<bool>("multi-line").unwrap_or(false))
                        .dot_matches_new_line(
                            s.get::<bool>("dot-matches-new-line").unwrap_or(false),
                        )
                        .crlf(s.get::<bool>("crlf").unwrap_or(false))
                        .line_terminator(s.get::<u8>("line-terminator").unwrap_or(b'\n'))
                        .swap_greed(s.get::<bool>("swap-greed").unwrap_or(false))
                        .ignore_whitespace(s.get::<bool>("ignore-whitespace").unwrap_or(false))
                        .octal(s.get::<bool>("octal").unwrap_or(false));

                    if let Ok(limit) = s.get::<u64>("size-limit") {
                        builder.size_limit(limit as usize);
                    }

                    if let Ok(limit) = s.get::<u64>("dfa-size-limit") {
                        builder.dfa_size_limit(limit as usize);
                    }

                    if let Ok(limit) = s.get::<u32>("nest-limit") {
                        builder.nest_limit(limit);
                    }

                    let regex = match builder.build() {
                        Ok(regex) => regex,
                        Err(err) => {
                            gst::error!(CAT, imp = self, "Failed to compile regex: {:?}", err);
                            continue;
                        }
                    };

                    let config = syntax::Config::new()
                        .unicode(s.get::<bool>("unicode").unwrap_or(true))
                        .case_insensitive(s.get::<bool>("case-insensitive").unwrap_or(false))
                        .multi_line(s.get::<bool>("multi-line").unwrap_or(false))
                        .dot_matches_new_line(
                            s.get::<bool>("dot-matches-new-line").unwrap_or(false),
                        )
                        .crlf(s.get::<bool>("crlf").unwrap_or(false))
                        .line_terminator(s.get::<u8>("line-terminator").unwrap_or(b'\n'))
                        .swap_greed(s.get::<bool>("swap-greed").unwrap_or(false))
                        .ignore_whitespace(s.get::<bool>("ignore-whitespace").unwrap_or(false))
                        .octal(s.get::<bool>("octal").unwrap_or(false));

                    let pattern = format!(r"^[[:^blank:]]*{}", pattern);

                    let dfa = match DFA::builder().syntax(config).build(&pattern) {
                        Ok(dfa) => dfa,
                        Err(err) => {
                            gst::error!(CAT, imp = self, "Failed to compile regex: {:?}", err);
                            continue;
                        }
                    };

                    let dfa_cache = dfa.create_cache();

                    match operation.as_str() {
                        "replace-all" | "replace_all" => {
                            let replacement = match s.get::<Option<String>>("replacement") {
                                Ok(Some(pattern)) => pattern,
                                Ok(None) | Err(_) => {
                                    gst::error!(
                                        CAT,
                                        imp = self,
                                        "Replace operations require a replacement field as a string"
                                    );
                                    continue;
                                }
                            };
                            state.commands.push(Command {
                                pattern,
                                regex,
                                dfa,
                                dfa_cache,
                                operation: Operation::ReplaceAll(replacement),
                            });
                        }
                        val => {
                            gst::error!(CAT, imp = self, "Unknown operation {}", val);
                        }
                    }
                }
            }
            "multi-buffer-mode" => {
                self.settings.lock().unwrap().multi_buffer_mode =
                    value.get().expect("type checked upstream");
            }
            _ => unimplemented!(),
        }
    }

    fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
        match pspec.name() {
            "commands" => {
                let state = self.state.lock().unwrap();
                let mut commands = gst::Array::default();
                for command in &state.commands {
                    match command.operation {
                        Operation::ReplaceAll(ref replacement) => {
                            commands.append(
                                gst::Structure::builder("replace-all")
                                    .field("pattern", &command.pattern)
                                    .field("replacement", replacement)
                                    .build(),
                            );
                        }
                    }
                }
                commands.to_value()
            }
            "multi-buffer-mode" => self.settings.lock().unwrap().multi_buffer_mode.to_value(),
            _ => unimplemented!(),
        }
    }
}

impl GstObjectImpl for RegEx {}

impl ElementImpl for RegEx {
    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
            gst::subclass::ElementMetadata::new(
                "Regular Expression processor",
                "Text/Filter",
                "Applies operations according to regular expressions",
                "Mathieu Duponchelle <mathieu@centricular.com>",
            )
        });

        Some(&*ELEMENT_METADATA)
    }

    fn change_state(
        &self,
        transition: gst::StateChange,
    ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
        if matches!(transition, gst::StateChange::ReadyToPaused) {
            self.state.lock().unwrap().settings = self.settings.lock().unwrap().clone();
        }

        self.parent_change_state(transition)
    }

    fn pad_templates() -> &'static [gst::PadTemplate] {
        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
            let caps = gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build();
            let src_pad_template = gst::PadTemplate::new(
                "src",
                gst::PadDirection::Src,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            let sink_pad_template = gst::PadTemplate::new(
                "sink",
                gst::PadDirection::Sink,
                gst::PadPresence::Always,
                &caps,
            )
            .unwrap();

            vec![src_pad_template, sink_pad_template]
        });

        PAD_TEMPLATES.as_ref()
    }
}
