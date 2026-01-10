// SPDX-License-Identifier: MPL-2.0

use crate::CAT;
use gst::glib;
use gst::prelude::*;
use std::sync::Once;

static REGISTER_ACTIONS: Once = Once::new();

/// Find a sink element in the pipeline based on the given criteria
fn find_sink(
    pipeline: &gst::Pipeline,
    sink_name: Option<&str>,
    factory_name: Option<&str>,
    caps: Option<&gst::Caps>,
) -> Result<gst::Element, String> {
    let found_sink = pipeline.iterate_recurse().find(|element| {
        if !element.has_property_with_type("last-sample", gst::Sample::static_type()) {
            return false;
        }

        if let Some(name) = sink_name {
            return element.name() == name;
        }

        if let Some(factory_name) = factory_name {
            return element
                .factory()
                .is_some_and(|factory| factory.name() == factory_name);
        }

        // Check caps if specified
        if let Some(expected_caps) = caps {
            // Check all sink pads
            for pad in element.iterate_sink_pads() {
                let pad = match pad {
                    Ok(p) => p,
                    Err(_) => continue,
                };
                if let Some(pad_caps) = pad.current_caps() {
                    if expected_caps.can_intersect(&pad_caps) {
                        return true;
                    }
                }
            }
        }

        // If nothing is specified, just get the first matching sink
        true
    });

    found_sink.ok_or_else(|| "No matching sink found in pipeline".to_string())
}

fn validate_json_fields(json_str: &str, expected_fields: &gst::Structure) -> Result<(), String> {
    let json_value: serde_json::Value = serde_json::from_str(json_str)
        .map_err(|e| format!("Failed to parse QR code content as JSON: {}", e))?;

    for field_name in expected_fields.fields() {
        let expected_value = expected_fields.value(field_name).map_err(|e| {
            format!(
                "Failed to get expected value for field '{}': {}",
                field_name, e
            )
        })?;

        // Serialize the expected GValue to JSON string, then deserialize to serde_json::Value
        // for simple comparison
        let expected_json: serde_json::Value = serde_json::from_str(
            expected_value
                .serialize()
                .map_err(|e| {
                    format!(
                        "Failed to serialize expected value for field '{}': {}",
                        field_name, e
                    )
                })?
                .as_str(),
        )
        .map_err(|e| {
            format!(
                "Failed to parse expected value as JSON for field '{}': {}",
                field_name, e
            )
        })?;

        let json_value = json_value
            .get(field_name.as_str())
            .ok_or_else(|| format!("Field '{}' not found in JSON", field_name))?;

        if json_value != &expected_json {
            return Err(format!(
                "JSON field '{}' mismatch: expected '{}', got '{}'",
                field_name, expected_json, json_value
            ));
        }
    }

    Ok(())
}

fn decode_qrcodes_from_sample(sample: &gst::Sample) -> Result<Vec<String>, String> {
    let buffer_ref = sample.buffer().ok_or("Sample has no buffer")?;
    let caps = sample.caps().ok_or("Sample has no caps")?;

    let in_info = gst_video::VideoInfo::from_caps(caps)
        .map_err(|_| "Failed to parse video info from caps")?;

    let width = in_info.width();
    let height = in_info.height();

    let out_info = gst_video::VideoInfo::builder(gst_video::VideoFormat::Gray8, width, height)
        .fps(in_info.fps())
        .build()
        .map_err(|_| "Failed to create GRAY8 VideoInfo")?;

    let converter = gst_video::VideoConverter::new(&in_info, &out_info, None)
        .map_err(|e| format!("Failed to create VideoConverter: {}", e))?;

    let in_frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer_ref, &in_info)
        .map_err(|e| format!("Failed to map input buffer as VideoFrame: {}", e))?;

    let gray_vec = vec![0u8; out_info.size()];
    let out_buffer = gst::Buffer::from_mut_slice(gray_vec);
    let mut out_frame = gst_video::VideoFrame::from_buffer_writable(out_buffer, &out_info)
        .map_err(|_| "Failed to map output buffer as writable VideoFrame")?;

    {
        let mut out_frame_ref = out_frame.as_mut_video_frame_ref();
        converter.frame_ref(&in_frame, &mut out_frame_ref);
    }

    let out_buffer = out_frame.into_buffer();

    let gray_vec = out_buffer
        .try_into_inner::<Vec<u8>>()
        .map_err(|_| "Failed to get buffer data")?;
    let gray_image = image::ImageBuffer::from_raw(width, height, gray_vec)
        .ok_or("Failed to create image buffer from gray data")?;

    let mut img = rqrr::PreparedImage::prepare(gray_image);
    let grids = img.detect_grids();

    // Decode all QR codes found
    let mut decoded_codes = Vec::new();
    for grid in grids {
        let (_meta, content) = grid
            .decode()
            .map_err(|e| format!("Failed to decode QR code: {:?}", e))?;
        decoded_codes.push(content);
    }

    Ok(decoded_codes)
}

fn check_last_frame_qrcode(
    scenario: &gst_validate::Scenario,
    action: &gst_validate::Action,
) -> Result<gst_validate::ActionSuccess, gst_validate::ActionError> {
    let structure = action.structure().unwrap();

    let pipeline = gst_validate::prelude::ScenarioExt::pipeline(scenario)
        .ok_or_else(|| gst_validate::ActionError::Error("No pipeline available".to_string()))?
        .downcast::<gst::Pipeline>()
        .expect("Pipeline element is not a gst::Pipeline");

    let sink_name = structure.get::<String>("sink-name").ok();
    let factory_name = structure.get::<String>("sink-factory-name").ok();
    let caps = structure.get::<gst::Caps>("sinkpad-caps").ok();

    let sink = find_sink(
        &pipeline,
        sink_name.as_deref(),
        factory_name.as_deref(),
        caps.as_ref(),
    )
    .map_err(gst_validate::ActionError::Error)?;

    let sample: gst::Sample = sink
        .property::<Option<gst::Sample>>("last-sample")
        .ok_or_else(|| {
            gst_validate::ActionError::Error(format!(
                "Could not get 'last-sample' from sink '{}'. \
                             Make sure the 'enable-last-sample' property is set to TRUE!",
                sink.name()
            ))
        })?;

    let decoded_codes =
        decode_qrcodes_from_sample(&sample).map_err(gst_validate::ActionError::Error)?;

    // Check if JSON field validation is requested
    let json_field_specs: Option<Vec<gst::Structure>> =
        match structure.get::<gst::Structure>("expected-json-fields") {
            Ok(single_spec) => Some(vec![single_spec]),
            _ => match structure.get::<gst::List>("expected-json-fields") {
                Ok(array) => Some(
                    array
                        .iter()
                        .filter_map(|v| v.get::<gst::Structure>().ok())
                        .collect(),
                ),
                _ => None,
            },
        };

    if let Some(expected_json_fields) = json_field_specs {
        if decoded_codes.len() != expected_json_fields.len() {
            return Err(gst_validate::ActionError::Error(format!(
                "JSON field validation: expected {} QR code(s), but found {} QR code(s): {:?}",
                expected_json_fields.len(),
                decoded_codes.len(),
                decoded_codes
            )));
        }

        // Validate each QR code against its corresponding field spec
        for (i, (qr_data, field_spec)) in decoded_codes
            .iter()
            .zip(expected_json_fields.iter())
            .enumerate()
        {
            validate_json_fields(qr_data, field_spec)
                .map_err(|e| gst_validate::ActionError::Error(format!("QR code #{}: {}", i, e)))?;
        }

        gst::debug!(
            CAT,
            obj = scenario,
            "Successfully validated {} QR code(s) with JSON field validation",
            decoded_codes.len()
        );
        return Ok(gst_validate::ActionSuccess::Ok);
    }

    // Get expected data - can be either a string or an array of strings
    let expected_values: Vec<String> = match structure.get::<String>("expected-data") {
        Ok(single_value) => {
            // Single string value (backwards compatible)
            vec![single_value]
        }
        _ => {
            match structure.get::<gst::List>("expected-data") {
                Ok(array) => {
                    // Array of strings
                    array
                        .iter()
                        .filter_map(|v| v.get::<String>().ok())
                        .collect()
                }
                _ => {
                    return Err(gst_validate::ActionError::Error(format!(
            "Either a string or an array of strings must be provided for 'expected-data' parameter, got {:?} \
                and expected-json-fields parameter is {:?}",
            structure.get::<glib::Value>("expected-data"),
            structure.get::<glib::Value>("expected-json-fields")
        )));
                }
            }
        }
    };

    if expected_values.is_empty() {
        if !decoded_codes.is_empty() {
            return Err(gst_validate::ActionError::Error(format!(
                "Expected no QR codes, but found {} QR code(s): {:?}",
                decoded_codes.len(),
                decoded_codes
            )));
        }
        gst::debug!(
            CAT,
            obj = scenario,
            "Successfully verified no QR codes in frame"
        );
        return Ok(gst_validate::ActionSuccess::Ok);
    }

    if decoded_codes.len() != expected_values.len() {
        return Err(gst_validate::ActionError::Error(format!(
            "QR code count mismatch: expected {} QR code(s) {:?}, but found {} QR code(s): {:?}",
            expected_values.len(),
            expected_values,
            decoded_codes.len(),
            decoded_codes
        )));
    }

    let mut sorted_decoded = decoded_codes.clone();
    let mut sorted_expected = expected_values.clone();
    sorted_decoded.sort();
    sorted_expected.sort();

    if sorted_decoded != sorted_expected {
        return Err(gst_validate::ActionError::Error(format!(
            "QR code data mismatch: expected {:?}, got {:?}",
            expected_values, decoded_codes
        )));
    }

    gst::debug!(
        CAT,
        obj = scenario,
        "Successfully validated {} QR code(s): {:?}",
        decoded_codes.len(),
        decoded_codes
    );

    Ok(gst_validate::ActionSuccess::Ok)
}

pub fn register_validate_actions(plugin: &gst::Plugin) -> Result<(), glib::BoolError> {
    REGISTER_ACTIONS.call_once(|| {
        gst_validate::ActionTypeBuilder::new(
            "check-last-frame-qrcode",
            |scenario, action| check_last_frame_qrcode(scenario, action)
        )
        .implementer_namespace(plugin.name().as_str())
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "sink-name",
                "The name of the sink element to check sample on",
            )
            .add_type("string")
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "sink-factory-name",
                "The factory name of the sink element to check sample on",
            )
            .add_type("string")
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "sinkpad-caps",
                "The caps (as string) of the sink pad to check",
            )
            .add_type("GstCaps")
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "expected-data",
                "The expected QR code data content. Can be:\n\
                 - A single string to check for one QR code\n\
                 - An array of strings to check for multiple QR codes (exact match, order-agnostic)\n\
                 - An empty array to verify no QR codes are present\n\
                 Note: Cannot be used together with 'expected-json-fields'.",
            )
            .add_type("string")
            .add_type("GstValueArray")
            .build(),
        )
        .parameter(
            gst_validate::ActionParameterBuilder::new(
                "expected-json-fields",
                "JSON field validation specification. Can be:\n\
                 - A single GstStructure for validating one QR code's JSON fields\n\
                 - An array of GstStructures for validating multiple QR codes (one spec per QR code, matched by index)\n\
                 Each GstStructure contains field names and expected values to validate in the QR code's JSON content.\n\
                 Supports nested fields using dot notation (e.g., 'user.name').\n\
                 Field values can be strings, numbers, or booleans.\n\
                 Note: Cannot be used together with 'expected-data'.",
            )
            .add_type("GstStructure")
            .add_type("GstValueArray")
            .build(),
        )
        .description(
            "Checks that QR codes in the last frame of the specified sink contain the expected data. \
             Supports two validation modes:\n\
             1. Exact string matching (expected-data): Validates single QR codes, multiple QR codes, or verifying no QR codes are present.\n\
             2. JSON field matching (expected-json-fields): Parses QR code as JSON and validates specific fields (supports nested fields with dot notation).\n\
             This allows validating QR code generation in video streams."
        )
        .flags(gst_validate::ActionTypeFlags::CHECK)
        .build();
    });

    Ok(())
}
