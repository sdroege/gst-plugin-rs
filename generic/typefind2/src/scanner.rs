// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

//! Typefinding engine: runs all registered `GstTypeFindFactory` against a byte
//! source and keeps the most confident suggestion.
//!
//! Rust counterpart of the scanning logic in `libs/gst/base/gsttypefindhelper.c`.

use gst::prelude::*;

/// How many bytes to read ahead when pulling data for a single `peek`, so that
/// factories scanning byte-by-byte do not trigger one pull per byte.
const READ_AHEAD: u32 = 4096;

/// Provides the bytes a round of typefinding inspects.
pub trait ByteSource {
    /// Returns exactly `size` bytes at `offset`, or `None` when they are not
    /// (yet) available. A negative offset counts from the end of the data.
    fn peek(&mut self, offset: i64, size: u32) -> Option<&[u8]>;

    /// Total data length, `None` when unknown.
    fn length(&self) -> Option<u64>;

    /// Flow error encountered while reading, if any. Checked after each
    /// factory: any error but `EOS` aborts the round and discards results.
    fn flow_error(&self) -> Result<(), gst::FlowError> {
        Ok(())
    }
}

/// Byte source over a fixed slice of memory.
pub struct SliceSource<'a> {
    data: &'a [u8],
}

impl<'a> SliceSource<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data }
    }
}

impl ByteSource for SliceSource<'_> {
    fn peek(&mut self, offset: i64, size: u32) -> Option<&[u8]> {
        if size == 0 {
            return None;
        }

        // End-relative peeks are supported: the whole slice is available.
        let start = if offset >= 0 {
            usize::try_from(offset).ok()?
        } else {
            self.data
                .len()
                .checked_sub(offset.unsigned_abs() as usize)?
        };
        let end = start.checked_add(usize::try_from(size).ok()?)?;
        self.data.get(start..end)
    }

    fn length(&self) -> Option<u64> {
        Some(self.data.len() as u64)
    }
}

/// Byte source over a pull-mode pad, with a cache of everything read: pointers
/// returned by `peek` must stay valid until the round is over.
pub struct PullSource {
    pad: gst::Pad,
    size: u64,
    cache: Vec<CacheEntry>,
    flow_error: Option<gst::FlowError>,
}

struct CacheEntry {
    offset: u64,
    data: Vec<u8>,
}

impl PullSource {
    pub fn new(pad: &gst::Pad, size: u64) -> Self {
        Self {
            pad: pad.clone(),
            size,
            cache: Vec::new(),
            flow_error: None,
        }
    }

    // The duration is queried before typefinding starts, a size of 0 or
    // `u64::MAX` means "unknown".
    fn known_size(&self) -> Option<u64> {
        match self.size {
            0 | u64::MAX => None,
            size => Some(size),
        }
    }
}

impl ByteSource for PullSource {
    fn peek(&mut self, offset: i64, size: u32) -> Option<&[u8]> {
        if size == 0 {
            return None;
        }

        let offset = if offset >= 0 {
            offset as u64
        } else {
            self.known_size()?.checked_sub(offset.unsigned_abs())?
        };

        // With a known size, a peek beyond the data can never succeed:
        // answer without reading. Reads at or past EOF deliberately record
        // nothing; the end-of-data marker is raised once, at the end of the
        // scan round (see typefind_pull).
        if self
            .known_size()
            .is_some_and(|known| offset.saturating_add(u64::from(size)) > known)
        {
            return None;
        }

        // A strict `<` here would re-pull peeks ending exactly at a cached
        // buffer's end; accept `<=`.
        if let Some(index) = self
            .cache
            .iter()
            .position(|entry| offset >= entry.offset && offset + u64::from(size) <= entry.end())
        {
            return self.cache[index].slice(offset, size);
        }

        match self.pad.pull_range(offset, size.max(READ_AHEAD)) {
            Ok(buffer) => {
                let buffer_offset = buffer.offset();
                if buffer_offset != gst::format::Buffers::OFFSET_NONE && buffer_offset != offset {
                    return None;
                }

                if buffer.size() < usize::try_from(size).ok()? {
                    // Truncated read, only EOF can cause this. Always return
                    // either the full request or nothing.
                    return None;
                }

                let map = buffer.map_readable().ok()?;
                self.cache.push(CacheEntry {
                    offset,
                    data: map.as_slice().to_vec(),
                });

                self.cache.last()?.slice(offset, size)
            }
            Err(err) => {
                self.flow_error = Some(err);
                None
            }
        }
    }

    fn length(&self) -> Option<u64> {
        self.known_size()
    }

    fn flow_error(&self) -> Result<(), gst::FlowError> {
        self.flow_error.map_or(Ok(()), Err)
    }
}

impl CacheEntry {
    fn end(&self) -> u64 {
        self.offset + self.data.len() as u64
    }

    fn slice(&self, offset: u64, size: u32) -> Option<&[u8]> {
        let start = usize::try_from(offset - self.offset).ok()?;
        let end = start.checked_add(usize::try_from(size).ok()?)?;
        self.data.get(start..end)
    }
}

/// Collects the suggestions of one round of typefinding, keeping the best.
pub struct Scan<T: ByteSource> {
    source: T,
    best: Option<(gst::TypeFindProbability, gst::Caps)>,
}

impl<T: ByteSource> Scan<T> {
    pub fn new(source: T) -> Self {
        Self { source, best: None }
    }
}

impl<T: ByteSource> gst::TypeFindImpl for Scan<T> {
    fn peek(&mut self, offset: i64, size: u32) -> Option<&[u8]> {
        self.source.peek(offset, size)
    }

    fn suggest(&mut self, probability: gst::TypeFindProbability, caps: &gst::Caps) {
        // Suggestions at probability NONE carry no information (the initial
        // best is none).
        if probability == gst::TypeFindProbability::None {
            return;
        }
        if self
            .best
            .as_ref()
            .is_none_or(|(best, _)| *best < probability)
        {
            self.best = Some((probability, caps.clone()));
        }
    }

    fn length(&self) -> Option<u64> {
        self.source.length()
    }
}

/// Result of a round of typefinding.
pub struct ScanOutcome {
    /// Most confident suggestion, if any.
    pub best: Option<(gst::TypeFindProbability, gst::Caps)>,
    /// Flow error that aborted (or marked) the round, if any. `EOS` is
    /// converted to `ERROR` at the end of the round.
    pub flow_error: Result<(), gst::FlowError>,
}

/// Runs all registered factories against `source`, best rank first. Factories
/// advertising `extension` are moved to the front of the list so that they can
/// short-circuit the round.
pub fn run<T: ByteSource>(mut scan: Scan<T>, extension: Option<&str>) -> ScanOutcome {
    for factory in ordered_factories(extension) {
        factory.call_function(&mut scan);

        if scan
            .best
            .as_ref()
            .is_some_and(|(probability, _)| *probability >= gst::TypeFindProbability::Maximum)
        {
            // Something was found before any potential read error: the error
            // doesn't matter.
            return ScanOutcome {
                best: scan.best,
                flow_error: Ok(()),
            };
        }

        match scan.source.flow_error() {
            Ok(()) => (),
            // Reading the end of the data is not an error for the remaining
            // factories, only for the caller.
            Err(gst::FlowError::Eos) => (),
            Err(err) => {
                // Results collected so far might be worse than what the
                // factories we never reached would have found.
                return ScanOutcome {
                    best: None,
                    flow_error: Err(err),
                };
            }
        }
    }

    let flow_error = match scan.source.flow_error() {
        Err(gst::FlowError::Eos) => Err(gst::FlowError::Error),
        res => res,
    };

    ScanOutcome {
        best: scan.best,
        flow_error,
    }
}

/// All factories with function, ranked, with extension matches first (stable,
/// mirroring `prioritize_extension()`).
fn ordered_factories(extension: Option<&str>) -> Vec<gst::TypeFindFactory> {
    let factories: Vec<gst::TypeFindFactory> =
        gst::TypeFindFactory::factories().into_iter().collect();

    match extension {
        Some(extension) => {
            let (mut matching, others): (Vec<_>, Vec<_>) = factories
                .into_iter()
                .partition(|factory| has_extension(factory, extension));
            matching.extend(others);
            matching
        }
        None => factories,
    }
}

fn has_extension(factory: &gst::TypeFindFactory, extension: &str) -> bool {
    factory
        .extensions()
        .iter()
        .any(|candidate| candidate == extension)
}

/// Caps advertised by the first factory without a probe function that claims
/// `extension`. Used to guess the type of streams that could not be detected
/// from their content.
pub fn caps_for_extension(extension: &str) -> Option<gst::Caps> {
    gst::TypeFindFactory::factories()
        .into_iter()
        .filter(|factory| !factory.has_function() && has_extension(factory, extension))
        .find_map(|factory| factory.caps())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn magic_factory(
        name: &str,
        rank: gst::Rank,
        extension: &str,
        offset: i64,
        magic: &[u8],
        probability: gst::TypeFindProbability,
        caps: &str,
    ) {
        let caps = gst::Caps::builder(caps).build();
        let magic: Vec<u8> = magic.to_vec();
        gst::TypeFind::register(
            None,
            name,
            rank,
            Some(extension),
            Some(&caps.clone()),
            move |find| {
                if find.peek_var(offset, magic.len() as u32) == Some(&magic[..]) {
                    find.suggest(probability, &caps);
                }
            },
        )
        .unwrap();
    }

    // Tests share one process, so factories are registered once for all of
    // them.
    fn scanner_init() {
        static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
        INIT.get_or_init(|| {
            gst::init().unwrap();
            // Both match `DATA` below; the higher-ranked one must be run first.
            magic_factory(
                "tf2_test_high",
                gst::Rank::PRIMARY + 10,
                "tf2h",
                4,
                b"BBBB",
                gst::TypeFindProbability::Maximum,
                "test/tf2-high",
            );
            // Suggests below Maximum so that the round is not short-circuited.
            magic_factory(
                "tf2_test_likely",
                gst::Rank::PRIMARY + 5,
                "tf2k",
                0,
                b"DDDD",
                gst::TypeFindProbability::Likely,
                "test/tf2-likely",
            );
            magic_factory(
                "tf2_test_low",
                gst::Rank::PRIMARY,
                "tf2l",
                0,
                b"AAAA",
                gst::TypeFindProbability::Maximum,
                "test/tf2-low",
            );
        });
    }

    const DATA: &[u8] = b"AAAABBBB";

    /// Byte source over a fixed prefix that records `beyond_error` once a peek
    /// reaches past the data, like a pull hitting EOF or a read failure.
    struct StubSource {
        data: &'static [u8],
        beyond_error: gst::FlowError,
        flow_error: Option<gst::FlowError>,
    }

    impl ByteSource for StubSource {
        fn peek(&mut self, offset: i64, size: u32) -> Option<&[u8]> {
            let start = usize::try_from(offset).ok()?;
            let end = start.checked_add(usize::try_from(size).ok()?)?;
            let bytes = self.data.get(start..end);
            if bytes.is_none() {
                self.flow_error = Some(self.beyond_error);
            }
            bytes
        }

        fn length(&self) -> Option<u64> {
            Some(self.data.len() as u64)
        }

        fn flow_error(&self) -> Result<(), gst::FlowError> {
            self.flow_error.map_or(Ok(()), Err)
        }
    }

    #[test]
    fn slice_scan_prefers_rank() {
        scanner_init();

        // Both factories match, the higher-ranked one runs first and its
        // maximum-probability suggestion short-circuits the round.
        let outcome = run(Scan::new(SliceSource::new(DATA)), None);
        let (probability, caps) = outcome.best.expect("type found");
        assert_eq!(probability, gst::TypeFindProbability::Maximum);
        assert_eq!(caps.structure(0).unwrap().name(), "test/tf2-high");
    }

    #[test]
    fn extension_moves_factory_first() {
        scanner_init();

        // The lower-ranked factory matches the extension, so it runs first and
        // its maximum-probability suggestion wins.
        let outcome = run(Scan::new(SliceSource::new(DATA)), Some("tf2l"));
        let (_probability, caps) = outcome.best.expect("type found");
        assert_eq!(caps.structure(0).unwrap().name(), "test/tf2-low");
    }

    #[test]
    fn run_keeps_result_when_source_eos() {
        scanner_init();

        // `tf2_test_high` peeks past the 4-byte data and records EOS, the
        // `tf2_test_likely` factory still matches: the detection survives, the
        // EOS is only reported as an error to the caller.
        let outcome = run(
            Scan::new(StubSource {
                data: b"DDDD",
                beyond_error: gst::FlowError::Eos,
                flow_error: None,
            }),
            None,
        );
        let (probability, caps) = outcome.best.expect("type found");
        assert_eq!(probability, gst::TypeFindProbability::Likely);
        assert_eq!(caps.structure(0).unwrap().name(), "test/tf2-likely");
        assert_eq!(outcome.flow_error, Err(gst::FlowError::Error));
    }

    #[test]
    fn run_discards_result_on_fatal_error() {
        scanner_init();

        // The first factory hits a fatal read error: nothing collected so far
        // is kept, since later factories might have found a better match.
        let outcome = run(
            Scan::new(StubSource {
                data: b"DDDD",
                beyond_error: gst::FlowError::Error,
                flow_error: None,
            }),
            None,
        );
        assert!(outcome.best.is_none());
        assert_eq!(outcome.flow_error, Err(gst::FlowError::Error));
    }

    #[test]
    fn slice_scan_no_match() {
        scanner_init();

        let data = b"\xa5\xa5\xa5\xa5";
        let outcome = run(Scan::new(SliceSource::new(&data[..])), None);
        assert!(outcome.best.is_none());
        assert_eq!(outcome.flow_error, Ok(()));
    }

    #[test]
    fn extension_caps_skip_factories_with_function() {
        scanner_init();

        // The tf2_test_* factories claim extensions but have probe functions,
        // so they must not be considered for guessing.
        assert!(caps_for_extension("tf2l").is_none());

        // Core's typefindfunctions registers function-less factories for
        // extensions of formats detected as part of another container.
        let factory = gst::TypeFindFactory::factories()
            .into_iter()
            .find(|factory| {
                !factory.has_function()
                    && !factory.extensions().is_empty()
                    && factory.caps().is_some()
            })
            .expect("gst-plugins-base registers function-less factories with caps");
        let extension = factory.extensions()[0].clone();
        assert_eq!(caps_for_extension(&extension), factory.caps());
    }
}
