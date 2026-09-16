// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use gst::{glib, prelude::*};
use gst_analytics::{ModelInfo, ModelInfoTensorDirection, TensorDataType, TensorDimOrder};

use super::converter;

pub(super) struct Model {
    pub group_id: glib::Quark,
    pub config: converter::Config,
    pub ctc: bool,
    pub tensor_groups: gst::Structure,
}

impl Model {
    pub fn load(path: &str) -> Result<Self, String> {
        let model_info = ModelInfo::load_from_file(path).ok_or("Couldn't load modelinfo file")?;
        let group_id = model_info
            .quark_group_id()
            .ok_or("Missing tensor group-id")?;
        let inputs = model_info.tensor_names(ModelInfoTensorDirection::Input);

        // TODO: Make input selection configurable if there are multiple inputs
        let [input_name] = inputs.as_slice() else {
            return Err(format!("Expected one input tensor, found {}", inputs.len()));
        };

        let data_type = model_info
            .tensor_data_type(input_name)
            .ok_or("Missing or invalid input tensor data type")?;
        if !matches!(data_type, TensorDataType::Uint8 | TensorDataType::Float32) {
            return Err(format!(
                "Unsupported video tensor data type '{}'",
                data_type.name()
            ));
        }

        let dims = model_info
            .tensor_dims(input_name)
            .ok_or("Missing or invalid input tensor dimensions")?;

        if !(2..=4).contains(&dims.len()) || dims.iter().any(|&dim| dim == 0 || dim < -1) {
            return Err(format!("Invalid video tensor dimensions: {dims:?}"));
        }

        let input_id = model_info
            .quark_id(input_name)
            .ok_or("Missing input tensor id")?;

        if input_id.as_str().is_empty() {
            return Err("Empty input tensor id".into());
        }

        let dims_order = model_info.dims_order(input_name);

        let (caps, inferred_channels_first) = if let Some(caps) = model_info.input_caps(input_name)
        {
            (caps, None)
        } else {
            let (caps, channels_first) = Self::guess_caps(&dims, data_type)?;
            (caps, Some(channels_first))
        };

        let config = converter::Config::new(
            &model_info,
            input_name,
            &caps,
            inferred_channels_first,
            &dims,
            data_type,
            dims_order,
            input_id,
        )?;

        let tensor_groups = {
            // Tensor caps represent dynamic dimensions with zero.
            let dims_val = gst::Array::new(dims.iter().map(|&dim| dim.max(0).to_send_value()));

            let dims_order_str = match dims_order {
                TensorDimOrder::RowMajor => "row-major",
                TensorDimOrder::ColMajor => "col-major",
                _ => unreachable!(),
            };

            let tensor_caps = gst::Caps::builder("tensor/strided")
                .field("tensor-id", input_id.as_str())
                .field("dims", dims_val)
                .field("dims-order", dims_order_str)
                .field("type", data_type.name())
                .build();

            gst::Structure::builder("tensorgroups")
                .field(group_id.as_str(), gst::UniqueList::new([tensor_caps]))
                .build()
        };

        Ok(Self {
            group_id,
            config,
            ctc: group_id.as_str() == "ctc-text-recognition-out",
            tensor_groups,
        })
    }

    fn guess_caps(dims: &[i32], data_type: TensorDataType) -> Result<(gst::Caps, bool), String> {
        // Keep the same layout preference as onnxinference
        let shape = if dims.len() == 4 { &dims[1..] } else { dims };
        let (channels, channels_first) = match shape {
            [_, _] => (1, false),
            [c, _, _] if *c == 1 || *c == 3 => (*c, true),
            [_, _, c] if *c == 1 || *c == 3 => (*c, false),
            _ => return Err(format!("Cannot infer video caps from dimensions {dims:?}")),
        };

        let base = match (channels, channels_first) {
            (1, _) => "GRAY",
            (_, true) => "RGBP",
            (_, false) => "RGB",
        };

        let format = match data_type {
            TensorDataType::Uint8 if channels == 1 => "GRAY8".to_owned(),
            TensorDataType::Uint8 => base.to_owned(),
            TensorDataType::Float32 => {
                let endian = if cfg!(target_endian = "little") {
                    "LE"
                } else {
                    "BE"
                };
                format!("{base}_F32{endian}")
            }
            _ => return Err("Cannot infer video caps for this tensor type".into()),
        };

        Ok((
            gst::Caps::builder("video/x-raw")
                .field("format", format)
                .build(),
            channels_first,
        ))
    }
}
