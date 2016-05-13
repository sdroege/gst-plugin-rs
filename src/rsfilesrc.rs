use libc::{c_char};
use std::ffi::{CStr, CString};
use std::{ptr, mem};
use std::u64;
use std::slice;

#[derive(Debug)]
pub struct FileSrc {
    location: Option<String>,
}

#[repr(C)]
pub enum GstFlowReturn {
    Ok = 0,
    NotLinked = -1,
    Flushing = -2,
    Eos = -3,
    NotNegotiated = -4,
    Error = -5,
}

#[repr(C)]
pub enum GBoolean {
    False = 0,
    True = 1,
}

impl FileSrc {
    fn new() -> FileSrc {
        FileSrc { location: None }
    }
}

impl Drop for FileSrc {
    fn drop(&mut self) {
        println!("drop");
    }
}

#[no_mangle]
pub extern "C" fn filesrc_new() -> *mut FileSrc {
    let instance = Box::new(FileSrc::new());
    return Box::into_raw(instance);
}

#[no_mangle]
pub extern "C" fn filesrc_drop(ptr: *mut FileSrc) {
    unsafe { Box::from_raw(ptr) };
}

#[no_mangle]
pub extern "C" fn filesrc_set_location(ptr: *mut FileSrc, location_ptr: *const c_char) {
    let filesrc: &mut FileSrc = unsafe { &mut *ptr };

    if location_ptr.is_null() {
        filesrc.location = None;
    } else {
        let location = unsafe { CStr::from_ptr(location_ptr) };
        filesrc.location = Some(String::from(location.to_str().unwrap()));
    }
}

#[no_mangle]
pub extern "C" fn filesrc_get_location(ptr: *mut FileSrc) -> *mut c_char {
    let filesrc: &mut FileSrc = unsafe { &mut *ptr };

    match filesrc.location {
        Some(ref location) =>
            CString::new(location.clone().into_bytes()).unwrap().into_raw(),
        None =>
            ptr::null_mut()
    }
}

#[no_mangle]
pub extern "C" fn filesrc_fill(ptr: *mut FileSrc, data_ptr: *mut u8, data_len: usize) -> GstFlowReturn {
    let filesrc: &mut FileSrc = unsafe { &mut *ptr };

    println!("{:?}", filesrc);
    let mut data = unsafe { slice::from_raw_parts_mut(data_ptr, data_len) };

    for i in 0..data.len() - 1 {
        data[i] = 1;
    }

    return GstFlowReturn::Ok;
}

#[no_mangle]
pub extern "C" fn filesrc_get_size(ptr: *mut FileSrc) -> u64 {
    let filesrc: &mut FileSrc = unsafe { &mut *ptr };

    return u64::MAX;
}

#[no_mangle]
pub extern "C" fn filesrc_start(ptr: *mut FileSrc) -> GBoolean {
    let filesrc: &mut FileSrc = unsafe { &mut *ptr };

    match filesrc.location {
        None => GBoolean::False,
        Some(_) => GBoolean::True
    }
}

#[no_mangle]
pub extern "C" fn filesrc_stop(ptr: *mut FileSrc) -> GBoolean {
    let filesrc: &mut FileSrc = unsafe { &mut *ptr };

    return GBoolean::True;
}

#[no_mangle]
pub extern "C" fn filesrc_is_seekable(ptr: *mut FileSrc) -> GBoolean {
    let filesrc: &mut FileSrc = unsafe { &mut *ptr };

    return GBoolean::True;
}

