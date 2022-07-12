use std::collections::HashMap;

use gst::{glib, prelude::*};

pub fn gvalue_to_json(val: &gst::glib::Value) -> Option<serde_json::Value> {
    match val.type_() {
        glib::Type::STRING => Some(val.get::<String>().unwrap().into()),
        glib::Type::BOOL => Some(val.get::<bool>().unwrap().into()),
        glib::Type::I32 => Some(val.get::<i32>().unwrap().into()),
        glib::Type::U32 => Some(val.get::<u32>().unwrap().into()),
        glib::Type::I_LONG | glib::Type::I64 => Some(val.get::<i64>().unwrap().into()),
        glib::Type::U_LONG | glib::Type::U64 => Some(val.get::<u64>().unwrap().into()),
        glib::Type::F32 => Some(val.get::<f32>().unwrap().into()),
        glib::Type::F64 => Some(val.get::<f64>().unwrap().into()),
        _ => {
            if let Ok(s) = val.get::<gst::Structure>() {
                serde_json::to_value(
                    s.iter()
                        .filter_map(|(name, value)| {
                            gvalue_to_json(value).map(|value| (name.to_string(), value))
                        })
                        .collect::<HashMap<String, serde_json::Value>>(),
                )
                .ok()
            } else if let Ok(a) = val.get::<gst::Array>() {
                serde_json::to_value(
                    a.iter()
                        .filter_map(|value| gvalue_to_json(value))
                        .collect::<Vec<serde_json::Value>>(),
                )
                .ok()
            } else if let Some((_klass, values)) = gst::glib::FlagsValue::from_value(val) {
                Some(
                    values
                        .iter()
                        .map(|value| value.nick())
                        .collect::<Vec<&str>>()
                        .join("+")
                        .into(),
                )
            } else if let Ok(value) = val.serialize() {
                Some(value.as_str().into())
            } else {
                None
            }
        }
    }
}

fn json_to_gststructure(val: &serde_json::Value) -> Option<glib::SendValue> {
    match val {
        serde_json::Value::Bool(v) => Some(v.to_send_value()),
        serde_json::Value::Number(n) => {
            if n.is_u64() {
                Some(n.as_u64().unwrap().to_send_value())
            } else if n.is_i64() {
                Some(n.as_i64().unwrap().to_send_value())
            } else if n.is_f64() {
                Some(n.as_f64().unwrap().to_send_value())
            } else {
                todo!("Unhandled case {n:?}");
            }
        }
        serde_json::Value::String(v) => Some(v.to_send_value()),
        serde_json::Value::Array(v) => {
            let array = v
                .iter()
                .filter_map(json_to_gststructure)
                .collect::<Vec<glib::SendValue>>();
            Some(gst::Array::from_values(array).to_send_value())
        }
        serde_json::Value::Object(v) => Some(serialize_json_object(v).to_send_value()),
        _ => None,
    }
}

pub fn serialize_json_object(val: &serde_json::Map<String, serde_json::Value>) -> gst::Structure {
    let mut res = gst::Structure::new_empty("v");

    val.iter().for_each(|(k, v)| {
        if let Some(gvalue) = json_to_gststructure(v) {
            res.set_value(k, gvalue);
        }
    });

    res
}
