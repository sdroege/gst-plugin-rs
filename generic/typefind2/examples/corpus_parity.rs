// Copyright (C) 2026 Edward Hervey <edward@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

// Compare typefind2 detection against the core reference tool on real files.
//
// For each argument (file or directory walked recursively): the reference is
// `gst-typefind-1.0` (the C helper, no extension priority), the subject is a
// real filesrc ! typefind2 ! fakesink pipeline, result taken from the
// have-type signal. Divergences should be limited to extension-based
// prioritization/guessing; anything else is a bug in the element glue.
//
// Run from the workspace root:
//   cargo run -p gst-plugin-typefind2 --example corpus_parity -- <file-or-dir>...

use gst::prelude::*;
use gsttypefind2::TypeFind2;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

fn collect_files(arg: &str, files: &mut Vec<PathBuf>) {
    let path = Path::new(arg);
    if path.is_dir() {
        for entry in std::fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            collect_files(entry.path().to_str().unwrap(), files);
        }
    } else if path.is_file() {
        files.push(path.to_path_buf());
    }
}

/// ((probability, caps) from have-type or None, whether the pipeline
/// completed on EOS).
fn detect_with_element(path: &Path) -> Result<(Option<(u32, String)>, bool), String> {
    let pipeline = gst::Pipeline::builder().build();
    let src = gst::ElementFactory::make("filesrc")
        .property("location", path.to_str().unwrap())
        .build()
        .unwrap();
    let tf = gst::ElementFactory::make("typefind2")
        .build()
        .unwrap()
        .downcast::<TypeFind2>()
        .unwrap();
    let sink = gst::ElementFactory::make("fakesink")
        .property("num-buffers", 1i32)
        .build()
        .unwrap();
    pipeline.add_many([&src, tf.as_ref(), &sink]).unwrap();
    src.link(&tf).unwrap();
    tf.link(&sink).unwrap();

    let found: Arc<Mutex<Option<(u32, String)>>> = Arc::new(Mutex::new(None));
    tf.connect_have_type({
        let found = found.clone();
        move |_, probability, caps| {
            let mut found = found.lock().unwrap();
            found.get_or_insert((probability, caps.to_string()));
        }
    });

    pipeline.set_state(gst::State::Playing).ok();
    let (res, _, _) = pipeline.state(Some(gst::ClockTime::from_seconds(15)));
    let mut error = None;
    if res.is_err() {
        error = pipeline
            .bus()
            .unwrap()
            .timed_pop_filtered(
                Some(gst::ClockTime::from_mseconds(100)),
                &[gst::MessageType::Error],
            )
            .map(|msg| match msg.view() {
                gst::MessageView::Error(err) => err.error().to_string(),
                _ => String::new(),
            });
    }

    let saw_eos = pipeline
        .bus()
        .unwrap()
        .timed_pop_filtered(
            Some(gst::ClockTime::from_seconds(10)),
            &[gst::MessageType::Eos],
        )
        .is_some();
    pipeline.set_state(gst::State::Null).ok();

    let found = (*found.lock().unwrap()).clone();
    match (found, error) {
        // Detection implies streaming resumed and the limited sink hit its
        // buffer count, ending in EOS.
        (Some(found), _) => Ok((Some(found), saw_eos)),
        // The element's own two "no type" diagnostics.
        (None, Some(err))
            if err.contains("determine type") || err.contains("contain enough data") =>
        {
            Ok((None, saw_eos))
        }
        (None, Some(err)) => Err(err),
        (None, None) => Err("no have-type and no error within timeout".to_string()),
    }
}

/// `gst-typefind-1.0` prints `<arg> - <caps>` per found type, nothing when
/// the helper finds none.
fn detect_with_reference(path: &Path) -> Result<Option<String>, String> {
    let output = Command::new("gst-typefind-1.0")
        .arg(path)
        .output()
        .map_err(|err| err.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("{} - ", path.display());
    Ok(stdout
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_string)))
}

fn main() -> Result<(), String> {
    gst::init().unwrap();
    gsttypefind2::plugin_register_static().unwrap();

    let mut files = Vec::new();
    for arg in std::env::args().skip(1) {
        collect_files(&arg, &mut files);
    }
    files.sort();

    let mut counts: std::collections::BTreeMap<String, u32> = Default::default();
    for path in &files {
        let name = path.file_name().unwrap().to_string_lossy();
        let (verdict, detail) = match (detect_with_element(path), detect_with_reference(path)) {
            (Ok((ours, saw_eos)), Ok(reference)) => {
                let mut verdict = classify(ours.as_ref(), reference.as_deref());
                if ours.is_some() && !saw_eos {
                    verdict = (
                        "MISMATCH".to_string(),
                        format!("detected but pipeline never reached EOS: {}", verdict.1),
                    );
                }
                verdict
            }
            (Err(err), _) => ("ERROR".to_string(), format!("element: {err}")),
            (_, Err(err)) => ("ERROR".to_string(), format!("reference: {err}")),
        };
        *counts.entry(verdict.clone()).or_default() += 1;
        println!("{verdict:15} {name:50} {detail}");
    }

    println!();
    for (verdict, count) in &counts {
        println!("{verdict}={count}");
    }
    let failures = counts
        .keys()
        .filter(|k| k.starts_with("MISMATCH") || *k == "ERROR")
        .count();
    if failures > 0 {
        return Err(format!("{failures} file(s) diverged"));
    }
    Ok(())
}

fn classify(ours: Option<&(u32, String)>, reference: Option<&str>) -> (String, String) {
    match (ours, reference) {
        (None, None) => ("MATCH-none".into(), "-".into()),
        (None, Some(ref_caps)) => ("MISMATCH".into(), format!("missed: {ref_caps}")),
        (Some((prob, our_caps)), None) => (
            "DIFF-unknown".into(),
            format!("only element found: {prob}% {our_caps}"),
        ),
        (Some((prob, our_caps)), Some(ref_caps)) => {
            let equal = our_caps == ref_caps
                || matches!(
                    (our_caps.parse::<gst::Caps>(), ref_caps.parse::<gst::Caps>()),
                    (Ok(ours), Ok(reference)) if ours == reference
                );
            if equal {
                ("MATCH".into(), format!("{prob}% {our_caps}"))
            } else if *prob >= 100 {
                // Both Maximum with the same engine: only extension
                // prioritization may pick a different factory.
                (
                    "DIFF-extension".into(),
                    format!("element: {prob}% {our_caps} | reference: {ref_caps}"),
                )
            } else {
                (
                    "MISMATCH".into(),
                    format!("element: {prob}% {our_caps} | reference: {ref_caps}"),
                )
            }
        }
    }
}
