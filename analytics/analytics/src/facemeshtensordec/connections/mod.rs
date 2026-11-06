// Copyright (C) 2026 Collabora Ltd
// Author: Mehdi Hassan <mehdi.hassan@collabora.com>
//
// This Source Code Form is subject to the terms of the Mozilla Public License, v2.0.
// If a copy of the MPL was not distributed with this file, You can obtain one at
// <https://mozilla.org/MPL/2.0/>.
//
// SPDX-License-Identifier: MPL-2.0

mod raw;

use raw::*;

use std::sync::LazyLock;

pub(super) static V1_SKELETON_PAIRS: LazyLock<&'static [i32]> =
    LazyLock::new(|| &SKELETON_PAIRS[IRIS_END..CONTOUR_END]);

pub(super) static V1_SKELETON_PAIRS_WITH_TESSELLATION: LazyLock<&'static [i32]> =
    LazyLock::new(|| &SKELETON_PAIRS[IRIS_END..]);

pub(super) static V2_SKELETON_PAIRS: LazyLock<&'static [i32]> =
    LazyLock::new(|| &SKELETON_PAIRS[..CONTOUR_END]);

pub(super) static V2_SKELETON_PAIRS_WITH_TESSELLATION: LazyLock<&'static [i32]> =
    LazyLock::new(|| &SKELETON_PAIRS[..]);

macro_rules! symmetric {
    ($($a:ident),*) => {
        &[$($a.as_flattened(), inverse($a).as_flattened(),)*]
    };
}

macro_rules! symmetric_len {
    ($a:ident) => {
        $a.as_flattened().len() + inverse($a).as_flattened().len()
    };
}

const fn flatten<const N: usize>(pairs: &[&[i32]]) -> [i32; N] {
    let mut result = [0; N];
    let mut result_i = 0;

    let mut i = 0;
    while i < pairs.len() {
        let mut j = 0;
        while j < pairs[i].len() {
            result[result_i] = pairs[i][j];
            result_i += 1;
            j += 1;
        }
        i += 1;
    }

    if N != result_i {
        // Rust compiler will error telling us what the true size of result is
        let _ = &[(); 1][result_i];
    }

    result
}

const fn inverse<const N: usize>(a: [Pair; N]) -> [Pair; N] {
    let mut result = [[0, 0]; N];
    let mut i = 0;

    while i < N {
        let [x, y] = a[i];
        result[i] = [y, x];
        i += 1;
    }

    result
}

static SKELETON_PAIRS: [i32; 5916] = {
    flatten(symmetric!(
        LEFT_IRIS,
        RIGHT_IRIS,
        LIPS,
        LEFT_EYE,
        RIGHT_EYE,
        LEFT_EYEBROW,
        RIGHT_EYEBROW,
        FACE_OVAL,
        NOSE,
        TESSELLATION
    ))
};

const IRIS_END: usize = symmetric_len!(LEFT_IRIS) + symmetric_len!(RIGHT_IRIS);
const CONTOUR_END: usize = IRIS_END
    + symmetric_len!(LIPS)
    + symmetric_len!(LEFT_EYE)
    + symmetric_len!(RIGHT_EYE)
    + symmetric_len!(LEFT_EYEBROW)
    + symmetric_len!(RIGHT_EYEBROW)
    + symmetric_len!(FACE_OVAL)
    + symmetric_len!(NOSE);
