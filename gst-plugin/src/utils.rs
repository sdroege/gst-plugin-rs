// Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use libc::c_char;
use std::ffi::CString;
use std::i32;
use num_rational::Rational32;

use gst;

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GstFlowReturn {
    Ok = 0,
    NotLinked = -1,
    Flushing = -2,
    Eos = -3,
    NotNegotiated = -4,
    Error = -5,
}

pub struct Element(*mut gst::GstElement);

impl Element {
    pub unsafe fn new(element: *mut gst::GstElement) -> Element {
        if element.is_null() {
            panic!("NULL not allowed");
        }

        gst::gst_object_ref(element as *mut gst::GstObject);

        Element(element)
    }

    pub unsafe fn as_ptr(&self) -> *mut gst::GstElement {
        self.0
    }
}

impl Drop for Element {
    fn drop(&mut self) {
        unsafe {
            gst::gst_object_unref(self.0 as *mut gst::GstObject);
        }
    }
}

impl Clone for Element {
    fn clone(&self) -> Self {
        unsafe { Element::new(self.0) }
    }
}

#[no_mangle]
pub unsafe extern "C" fn cstring_drop(ptr: *mut c_char) {
    let _ = CString::from_raw(ptr);
}

pub fn f64_to_fraction(val: f64) -> Option<Rational32> {
    // Continued fractions algorithm
    // http://mathforum.org/dr.math/faq/faq.fractions.html#decfrac

    let negative = val < 0.0;

    let mut q = val.abs();
    let mut n0 = 0;
    let mut d0 = 1;
    let mut n1 = 1;
    let mut d1 = 0;

    const MAX_ITERATIONS: usize = 30;
    const MAX_ERROR: f64 = 1.0e-20;
    // 1/EPSILON > i32::MAX
    const EPSILON: f64 = 1.0e-10;

    // Overflow
    if q > i32::MAX as f64 {
        return None;
    }

    for _ in 0..MAX_ITERATIONS {
        let a = q as u32;
        let f = q - (a as f64);

        // Prevent overflow
        if a != 0 &&
           (n1 > (i32::MAX as u32) / a || d1 > (i32::MAX as u32) / a ||
            a * n1 > (i32::MAX as u32) - n0 || a * d1 > (i32::MAX as u32) - d0) {
            break;
        }

        let n = a * n1 + n0;
        let d = a * d1 + d0;

        n0 = n1;
        d0 = d1;
        n1 = n;
        d1 = d;

        // Prevent division by ~0
        if f < EPSILON {
            break;
        }
        let r = 1.0 / f;

        // Simplify fraction. Doing so here instead of at the end
        // allows us to get closer to the target value without overflows
        let g = gcd(n1, d1);
        if g != 0 {
            n1 /= g;
            d1 /= g;
        }

        // Close enough?
        if ((n as f64) / (d as f64) - val).abs() < MAX_ERROR {
            break;
        }

        q = r;
    }

    // Guaranteed by the overflow check
    assert!(n1 <= i32::MAX as u32);
    assert!(d1 <= i32::MAX as u32);

    // Overflow
    if d1 == 0 {
        return None;
    }

    // Make negative again if needed
    if negative {
        Some(Rational32::new(-(n1 as i32), d1 as i32))
    } else {
        Some(Rational32::new(n1 as i32, d1 as i32))
    }
}

pub fn gcd(mut a: u32, mut b: u32) -> u32 {
    // Euclid's algorithm
    while b != 0 {
        let tmp = a;
        a = b;
        b = tmp % b;
    }

    a
}

#[cfg(test)]
mod tests {
    use super::*;
    use num_rational::Rational32;

    #[test]
    fn test_gcd() {
        assert_eq!(gcd(2 * 2 * 2 * 2, 2 * 2 * 2 * 3), 2 * 2 * 2);
        assert_eq!(gcd(2 * 3 * 5 * 5 * 7, 2 * 5 * 7), 2 * 5 * 7);
    }

    #[test]
    fn test_f64_to_fraction() {
        assert_eq!(f64_to_fraction(2.0), Some(Rational32::new(2, 1)));
        assert_eq!(f64_to_fraction(2.5), Some(Rational32::new(5, 2)));
        assert_eq!(f64_to_fraction(0.127659574),
                   Some(Rational32::new(29013539, 227272723)));
        assert_eq!(f64_to_fraction(29.97), Some(Rational32::new(2997, 100)));
    }
}
