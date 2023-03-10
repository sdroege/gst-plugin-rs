// Copyright (C) 2020 Mathieu Duponchelle <mathieu@centricular.com>
// Copyright (C) 2023 François Laignel <francois@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::subclass::prelude::*;
use gst::{glib, prelude::*};

use aws_sdk_transcribestreaming as aws_transcribe;
use aws_sdk_transcribestreaming::model;

use futures::channel::mpsc;
use futures::prelude::*;
use tokio::sync::broadcast;

use std::sync::Arc;

use super::imp::{Settings, Transcriber};
use super::CAT;

#[derive(Debug)]
pub struct TranscriptionSettings {
    lang_code: model::LanguageCode,
    sample_rate: i32,
    vocabulary: Option<String>,
    vocabulary_filter: Option<String>,
    vocabulary_filter_method: model::VocabularyFilterMethod,
    session_id: Option<String>,
    results_stability: model::PartialResultsStability,
}

impl TranscriptionSettings {
    pub(super) fn from(settings: &Settings, sample_rate: i32) -> Self {
        TranscriptionSettings {
            lang_code: settings.language_code.as_str().into(),
            sample_rate,
            vocabulary: settings.vocabulary.clone(),
            vocabulary_filter: settings.vocabulary_filter.clone(),
            vocabulary_filter_method: settings.vocabulary_filter_method.into(),
            session_id: settings.session_id.clone(),
            results_stability: settings.results_stability.into(),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TranscriptItem {
    pub pts: gst::ClockTime,
    pub duration: gst::ClockTime,
    pub content: String,
    pub is_punctuation: bool,
}

impl TranscriptItem {
    pub fn from(item: model::Item, lateness: gst::ClockTime) -> Option<TranscriptItem> {
        let content = item.content?;

        let start_time = ((item.start_time * 1_000_000_000.0) as u64).nseconds() + lateness;
        let end_time = ((item.end_time * 1_000_000_000.0) as u64).nseconds() + lateness;

        Some(TranscriptItem {
            pts: start_time,
            duration: end_time - start_time,
            content,
            is_punctuation: matches!(item.r#type, Some(model::ItemType::Punctuation)),
        })
    }

    #[inline]
    pub fn push(&mut self, item: &TranscriptItem) {
        self.duration += item.duration;

        self.is_punctuation &= item.is_punctuation;
        if !item.is_punctuation {
            self.content.push(' ');
        }

        self.content.push_str(&item.content);
    }
}

#[derive(Clone)]
pub enum TranscriptEvent {
    Items(Arc<Vec<TranscriptItem>>),
    Eos,
}

impl From<Vec<TranscriptItem>> for TranscriptEvent {
    fn from(transcript_items: Vec<TranscriptItem>) -> Self {
        TranscriptEvent::Items(transcript_items.into())
    }
}

pub struct TranscriberLoop {
    imp: glib::subclass::ObjectImplRef<Transcriber>,
    client: aws_transcribe::Client,
    settings: Option<TranscriptionSettings>,
    lateness: gst::ClockTime,
    buffer_rx: Option<mpsc::Receiver<gst::Buffer>>,
    transcript_items_tx: broadcast::Sender<TranscriptEvent>,
    partial_index: usize,
}

impl TranscriberLoop {
    pub fn new(
        imp: &Transcriber,
        settings: TranscriptionSettings,
        lateness: gst::ClockTime,
        buffer_rx: mpsc::Receiver<gst::Buffer>,
        transcript_items_tx: broadcast::Sender<TranscriptEvent>,
    ) -> Self {
        let aws_config = imp.aws_config.lock().unwrap();
        let aws_config = aws_config
            .as_ref()
            .expect("aws_config must be initialized at this stage");

        TranscriberLoop {
            imp: imp.ref_counted(),
            client: aws_transcribe::Client::new(aws_config),
            settings: Some(settings),
            lateness,
            buffer_rx: Some(buffer_rx),
            transcript_items_tx,
            partial_index: 0,
        }
    }

    pub async fn run(mut self) -> Result<(), gst::ErrorMessage> {
        // Stream the incoming buffers chunked
        let chunk_stream = self.buffer_rx.take().unwrap().flat_map(move |buffer: gst::Buffer| {
            async_stream::stream! {
                let data = buffer.map_readable().unwrap();
                use aws_transcribe::{model::{AudioEvent, AudioStream}, types::Blob};
                for chunk in data.chunks(8192) {
                    yield Ok(AudioStream::AudioEvent(AudioEvent::builder().audio_chunk(Blob::new(chunk)).build()));
                }
            }
        });

        let settings = self.settings.take().unwrap();
        let mut transcribe_builder = self
            .client
            .start_stream_transcription()
            .language_code(settings.lang_code)
            .media_sample_rate_hertz(settings.sample_rate)
            .media_encoding(model::MediaEncoding::Pcm)
            .enable_partial_results_stabilization(true)
            .partial_results_stability(settings.results_stability)
            .set_vocabulary_name(settings.vocabulary)
            .set_session_id(settings.session_id);

        if let Some(vocabulary_filter) = settings.vocabulary_filter {
            transcribe_builder = transcribe_builder
                .vocabulary_filter_name(vocabulary_filter)
                .vocabulary_filter_method(settings.vocabulary_filter_method);
        }

        let mut output = transcribe_builder
            .audio_stream(chunk_stream.into())
            .send()
            .await
            .map_err(|err| {
                let err = format!("Transcribe ws init error: {err}");
                gst::error!(CAT, imp: self.imp, "{err}");
                gst::error_msg!(gst::LibraryError::Init, ["{err}"])
            })?;

        while let Some(event) = output
            .transcript_result_stream
            .recv()
            .await
            .map_err(|err| {
                let err = format!("Transcribe ws stream error: {err}");
                gst::error!(CAT, imp: self.imp, "{err}");
                gst::error_msg!(gst::LibraryError::Failed, ["{err}"])
            })?
        {
            if let model::TranscriptResultStream::TranscriptEvent(transcript_evt) = event {
                let mut ready_items = None;

                if let Some(result) = transcript_evt
                    .transcript
                    .and_then(|transcript| transcript.results)
                    .and_then(|mut results| results.drain(..).next())
                {
                    gst::trace!(CAT, imp: self.imp, "Received: {result:?}");

                    if let Some(alternative) = result
                        .alternatives
                        .and_then(|mut alternatives| alternatives.drain(..).next())
                    {
                        ready_items = alternative.items.and_then(|items| {
                            self.get_ready_transcript_items(items, result.is_partial)
                        });
                    }
                }

                if let Some(ready_items) = ready_items {
                    if self.transcript_items_tx.send(ready_items.into()).is_err() {
                        gst::debug!(CAT, imp: self.imp, "No transcript items receivers");
                        break;
                    }
                }
            } else {
                gst::warning!(
                    CAT,
                    imp: self.imp,
                    "Transcribe ws returned unknown event: consider upgrading the SDK"
                )
            }
        }

        gst::debug!(CAT, imp: self.imp, "Transcriber loop sending EOS");
        let _ = self.transcript_items_tx.send(TranscriptEvent::Eos);

        gst::debug!(CAT, imp: self.imp, "Exiting transcriber loop");

        Ok(())
    }

    /// Builds a list from the provided stable items.
    fn get_ready_transcript_items(
        &mut self,
        mut items: Vec<model::Item>,
        partial: bool,
    ) -> Option<Vec<TranscriptItem>> {
        if items.len() <= self.partial_index {
            gst::error!(
                CAT,
                imp: self.imp,
                "sanity check failed, alternative length {} < partial_index {}",
                items.len(),
                self.partial_index
            );

            if !partial {
                self.partial_index = 0;
            }

            return None;
        }

        let mut output = vec![];

        for item in items.drain(self.partial_index..) {
            if !item.stable().unwrap_or(false) {
                break;
            }

            let Some(item) = TranscriptItem::from(item, self.lateness) else { continue };
            gst::debug!(
                CAT,
                imp: self.imp,
                "Item is ready for queuing: {}, PTS {}",
                item.content,
                item.pts,
            );

            self.partial_index += 1;
            output.push(item);
        }

        if !partial {
            self.partial_index = 0;
        }

        if output.is_empty() {
            return None;
        }

        Some(output)
    }
}
