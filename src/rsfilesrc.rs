use libc::{c_char};
use std::ffi::{CStr, CString};
use std::ptr;
use std::u64;

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
pub extern "C" fn filesrc_fill(ptr: *mut FileSrc) -> GstFlowReturn {
    let filesrc: &mut FileSrc = unsafe { &mut *ptr };

    println!("{:?}", filesrc);

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

    return GBoolean::True;
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

