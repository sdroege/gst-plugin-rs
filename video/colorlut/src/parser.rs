// Copyright (C) 2026 Seungha Yang <seungha@centricular.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

use std::fs;
use std::path::Path;

const LUT_1D_MIN_SIZE: usize = 2;
const LUT_1D_MAX_SIZE: usize = 65536;

const LUT_3D_MIN_SIZE: usize = 2;
const LUT_3D_MAX_SIZE: usize = 256;

#[derive(Debug, Clone)]
pub struct Lut3D {
    size: usize,
    rgba: Vec<[f32; 4]>,
}

impl Lut3D {
    fn new(size: usize, rgba: Vec<[f32; 4]>) -> Option<Self> {
        if rgba.len() != size * size * size {
            return None;
        }

        Some(Self { size, rgba })
    }

    pub fn size(&self) -> usize {
        self.size
    }

    #[allow(unused)]
    pub fn as_flat(&self) -> &[[f32; 4]] {
        &self.rgba
    }

    #[inline(always)]
    pub fn at(&self, x: usize, y: usize, z: usize) -> [f32; 4] {
        debug_assert!(x < self.size);
        debug_assert!(y < self.size);
        debug_assert!(z < self.size);

        unsafe {
            *self
                .rgba
                .get_unchecked(x + y * self.size + z * self.size * self.size)
        }
    }
}

#[derive(Debug, Clone)]
pub enum CubeLutKind {
    Lut1D {
        size: usize,
        r: Vec<f32>,
        g: Vec<f32>,
        b: Vec<f32>,
    },

    Lut3D(Lut3D),
}

#[derive(Debug, Clone)]
pub struct CubeLut {
    pub domain_scale: [f32; 3],
    pub domain_offset: [f32; 3],

    pub kind: CubeLutKind,
}

#[derive(Debug)]
pub enum CubeParseError {
    Io(std::io::Error),
    InvalidLut(String),
}

impl std::fmt::Display for CubeParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(err) => write!(f, "IO error: {err}"),
            Self::InvalidLut(msg) => write!(f, "Invalid LUT: {msg}"),
        }
    }
}

impl From<std::io::Error> for CubeParseError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

#[derive(Debug, Clone, Copy)]
enum ParseState {
    Header,
    Lut1D { size: usize },
    Lut3D { size: usize },
}

impl CubeLut {
    pub fn parse_file(path: impl AsRef<Path>) -> Result<Self, CubeParseError> {
        let text = fs::read_to_string(path)?;
        Self::parse(&text)
    }

    pub fn parse(text: &str) -> Result<Self, CubeParseError> {
        let mut domain_min = [0.0f32; 3];
        let mut domain_max = [1.0f32; 3];

        let mut state = ParseState::Header;
        let mut values = Vec::<[f32; 3]>::new();

        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            let line = raw_line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let mut parts = line.split_whitespace();

            let first = match parts.next() {
                Some(v) => v,
                None => continue,
            };

            match first {
                "TITLE" => {
                    ensure_header(state, line_no, line)?;
                }
                "DOMAIN_MIN" => {
                    ensure_header(state, line_no, line)?;
                    domain_min = parse_vec3(parts, line_no, line)?;
                }
                "DOMAIN_MAX" => {
                    ensure_header(state, line_no, line)?;
                    domain_max = parse_vec3(parts, line_no, line)?;
                }
                "LUT_1D_SIZE" => {
                    ensure_header(state, line_no, line)?;

                    let size = parse_single_usize(parts, line_no, line)?;
                    validate_lut_size(size, LUT_1D_MIN_SIZE, LUT_1D_MAX_SIZE, line_no)?;

                    state = ParseState::Lut1D { size };
                }
                "LUT_3D_SIZE" => {
                    ensure_header(state, line_no, line)?;

                    let size = parse_single_usize(parts, line_no, line)?;
                    validate_lut_size(size, LUT_3D_MIN_SIZE, LUT_3D_MAX_SIZE, line_no)?;

                    state = ParseState::Lut3D { size };
                }
                _ => {
                    match state {
                        ParseState::Header => {
                            return Err(CubeParseError::InvalidLut(format!(
                                "LUT data found before LUT size at line {line_no}: {line}"
                            )));
                        }
                        ParseState::Lut1D { .. } | ParseState::Lut3D { .. } => {}
                    }

                    let r = parse_f32(first, line_no, line)?;
                    let g = parse_next_f32(&mut parts, line_no, line)?;
                    let b = parse_next_f32(&mut parts, line_no, line)?;

                    if parts.next().is_some() {
                        return Err(CubeParseError::InvalidLut(format!(
                            "Invalid line {line_no}: {line}"
                        )));
                    }

                    values.push([r, g, b]);
                }
            }
        }

        if domain_min[0] >= domain_max[0]
            || domain_min[1] >= domain_max[1]
            || domain_min[2] >= domain_max[2]
        {
            return Err(CubeParseError::InvalidLut(format!(
                "Invalid domain min {domain_min:?}, max {domain_max:?}",
            )));
        }

        let kind = match state {
            ParseState::Header => {
                return Err(CubeParseError::InvalidLut("Missing LUT size".to_string()));
            }
            ParseState::Lut1D { size } => {
                if values.len() != size {
                    return Err(CubeParseError::InvalidLut(format!(
                        "Invalid 1D LUT value count, expected {size}, got {}",
                        values.len()
                    )));
                }

                let mut r = Vec::with_capacity(size);
                let mut g = Vec::with_capacity(size);
                let mut b = Vec::with_capacity(size);

                for value in values {
                    r.push(value[0]);
                    g.push(value[1]);
                    b.push(value[2]);
                }

                CubeLutKind::Lut1D { size, r, g, b }
            }
            ParseState::Lut3D { size } => {
                let expected = size
                    .checked_mul(size)
                    .and_then(|v| v.checked_mul(size))
                    .ok_or_else(|| {
                        CubeParseError::InvalidLut(format!("Too large 3D LUT size {size}"))
                    })?;

                if values.len() != expected {
                    return Err(CubeParseError::InvalidLut(format!(
                        "Invalid 3D LUT value count, expected {expected}, got {}",
                        values.len()
                    )));
                }

                let rgba = values
                    .into_iter()
                    .map(|v| [v[0], v[1], v[2], 1.0])
                    .collect();

                CubeLutKind::Lut3D(Lut3D::new(size, rgba).ok_or_else(|| {
                    CubeParseError::InvalidLut(format!("Invalid 3D LUT size {size}"))
                })?)
            }
        };

        let domain_scale = [
            1.0 / (domain_max[0] - domain_min[0]),
            1.0 / (domain_max[1] - domain_min[1]),
            1.0 / (domain_max[2] - domain_min[2]),
        ];

        let domain_offset = [
            -domain_min[0] * domain_scale[0],
            -domain_min[1] * domain_scale[1],
            -domain_min[2] * domain_scale[2],
        ];

        Ok(Self {
            domain_scale,
            domain_offset,
            kind,
        })
    }
}

fn ensure_header(state: ParseState, line_no: usize, line: &str) -> Result<(), CubeParseError> {
    match state {
        ParseState::Header => Ok(()),
        ParseState::Lut1D { .. } | ParseState::Lut3D { .. } => Err(CubeParseError::InvalidLut(
            format!("Header found after LUT data at line {line_no}: {line}"),
        )),
    }
}

fn validate_lut_size(
    size: usize,
    min: usize,
    max: usize,
    line_no: usize,
) -> Result<(), CubeParseError> {
    if !(min..=max).contains(&size) {
        return Err(CubeParseError::InvalidLut(format!(
            "Invalid LUT size {size} at line {line_no}, expected {min}..={max}"
        )));
    }

    Ok(())
}

fn parse_vec3<'a>(
    mut parts: impl Iterator<Item = &'a str>,
    line_no: usize,
    line: &str,
) -> Result<[f32; 3], CubeParseError> {
    let r = parse_next_f32(&mut parts, line_no, line)?;
    let g = parse_next_f32(&mut parts, line_no, line)?;
    let b = parse_next_f32(&mut parts, line_no, line)?;

    if parts.next().is_some() {
        return Err(CubeParseError::InvalidLut(format!(
            "Invalid line {line_no}: {line}"
        )));
    }

    Ok([r, g, b])
}

fn parse_single_usize<'a>(
    mut parts: impl Iterator<Item = &'a str>,
    line_no: usize,
    line: &str,
) -> Result<usize, CubeParseError> {
    let value = parts
        .next()
        .ok_or_else(|| CubeParseError::InvalidLut(format!("Invalid line {line_no}: {line}")))?
        .parse::<usize>()
        .map_err(|_| {
            CubeParseError::InvalidLut(format!("Invalid integer at line {line_no}: {line}"))
        })?;

    if parts.next().is_some() {
        return Err(CubeParseError::InvalidLut(format!(
            "Invalid line {line_no}: {line}"
        )));
    }

    Ok(value)
}

fn parse_next_f32<'a>(
    parts: &mut impl Iterator<Item = &'a str>,
    line_no: usize,
    line: &str,
) -> Result<f32, CubeParseError> {
    let text = parts
        .next()
        .ok_or_else(|| CubeParseError::InvalidLut(format!("Invalid line {line_no}: {line}")))?;

    parse_f32(text, line_no, line)
}

fn parse_f32(text: &str, line_no: usize, line: &str) -> Result<f32, CubeParseError> {
    text.parse::<f32>()
        .map_err(|_| CubeParseError::InvalidLut(format!("Invalid float at line {line_no}: {line}")))
}
