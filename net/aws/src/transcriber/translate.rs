// Copyright (C) 2023 François Laignel <francois@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::glib;
use gst::subclass::prelude::*;

use aws_sdk_translate as aws_translate;

use futures::channel::mpsc;
use futures::prelude::*;

use std::collections::VecDeque;

use super::imp::TranslationSrcPad;
use super::transcribe::TranscriptItem;
use super::CAT;

pub struct TranslatedItem {
    pub pts: gst::ClockTime,
    pub duration: gst::ClockTime,
    pub content: String,
}

impl From<&TranscriptItem> for TranslatedItem {
    fn from(transcript_item: &TranscriptItem) -> Self {
        TranslatedItem {
            pts: transcript_item.pts,
            duration: transcript_item.duration,
            content: transcript_item.content.clone(),
        }
    }
}

#[derive(Default)]
pub struct TranslationQueue {
    items: VecDeque<TranscriptItem>,
}

impl TranslationQueue {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Pushes the provided item.
    ///
    /// Returns `Some(..)` if items are ready for translation.
    pub fn push(&mut self, transcript_item: &TranscriptItem) -> Option<TranscriptItem> {
        // Keep track of the item individually so we can schedule translation precisely.
        self.items.push_back(transcript_item.clone());

        if transcript_item.is_punctuation {
            // This makes it a good chunk for translation.
            // Concatenate as a single item for translation

            let mut items = self.items.drain(..);

            let mut item_acc = items.next()?;
            for item in items {
                item_acc.push(&item);
            }

            item_acc.push(transcript_item);

            return Some(item_acc);
        }

        // Regular case: no separator detected, don't push transcript items
        // to translation now. They will be pushed either if a punctuation
        // is found or of a `dequeue()` is requested.

        None
    }

    /// Dequeues items from the specified `deadline` up to `lookahead`.
    ///
    /// Returns `Some(..)` with the accumulated items matching the criteria.
    pub fn dequeue(
        &mut self,
        deadline: gst::ClockTime,
        lookahead: gst::ClockTime,
    ) -> Option<TranscriptItem> {
        if self.items.front()?.pts < deadline {
            // First item is too early to be sent to translation now
            // we can wait for more items to accumulate.
            return None;
        }

        // Can't wait any longer to send the first item to translation
        // Try to get up to lookahead more items to improve translation accuracy
        let limit = deadline + lookahead;

        let mut item_acc = self.items.pop_front().unwrap();
        while let Some(item) = self.items.front() {
            if item.pts > limit {
                break;
            }

            let item = self.items.pop_front().unwrap();
            item_acc.push(&item);
        }

        Some(item_acc)
    }
}

pub struct TranslationLoop {
    pad: glib::subclass::ObjectImplRef<TranslationSrcPad>,
    client: aws_translate::Client,
    input_lang: String,
    output_lang: String,
    transcript_rx: mpsc::Receiver<TranscriptItem>,
    translation_tx: mpsc::Sender<TranslatedItem>,
}

impl TranslationLoop {
    pub fn new(
        imp: &super::imp::Transcriber,
        pad: &TranslationSrcPad,
        input_lang: &str,
        output_lang: &str,
        transcript_rx: mpsc::Receiver<TranscriptItem>,
        translation_tx: mpsc::Sender<TranslatedItem>,
    ) -> Self {
        let aws_config = imp.aws_config.lock().unwrap();
        let aws_config = aws_config
            .as_ref()
            .expect("aws_config must be initialized at this stage");

        TranslationLoop {
            pad: pad.ref_counted(),
            client: aws_sdk_translate::Client::new(aws_config),
            input_lang: input_lang.to_string(),
            output_lang: output_lang.to_string(),
            transcript_rx,
            translation_tx,
        }
    }

    pub async fn check_language(&self) -> Result<(), gst::ErrorMessage> {
        let language_list = self.client.list_languages().send().await.map_err(|err| {
            let err = format!("Failed to call list_languages service: {err}");
            gst::info!(CAT, imp: self.pad, "{err}");
            gst::error_msg!(gst::LibraryError::Failed, ["{err}"])
        })?;

        let found_output_lang = language_list
            .languages()
            .and_then(|langs| {
                langs
                    .iter()
                    .find(|lang| lang.language_code() == Some(&self.output_lang))
            })
            .is_some();

        if !found_output_lang {
            let err = format!("Unknown output languages: {}", self.output_lang);
            gst::info!(CAT, imp: self.pad, "{err}");
            return Err(gst::error_msg!(gst::LibraryError::Failed, ["{err}"]));
        }

        Ok(())
    }

    pub async fn run(mut self) -> Result<(), gst::ErrorMessage> {
        while let Some(transcript_item) = self.transcript_rx.next().await {
            let TranscriptItem {
                pts,
                duration,
                content,
                ..
            } = transcript_item;

            let translated_text = if content.is_empty() {
                content
            } else {
                self.client
                    .translate_text()
                    .set_source_language_code(Some(self.input_lang.clone()))
                    .set_target_language_code(Some(self.output_lang.clone()))
                    .set_text(Some(content))
                    .send()
                    .await
                    .map_err(|err| {
                        let err = format!("Failed to call translation service: {err}");
                        gst::info!(CAT, imp: self.pad, "{err}");
                        gst::error_msg!(gst::LibraryError::Failed, ["{err}"])
                    })?
                    .translated_text
                    .unwrap_or_default()
            };

            let translated_item = TranslatedItem {
                pts,
                duration,
                content: translated_text,
            };

            if self.translation_tx.send(translated_item).await.is_err() {
                gst::info!(
                    CAT,
                    imp: self.pad,
                    "translation chan terminated, exiting translation loop"
                );
                break;
            }
        }

        Ok(())
    }
}
