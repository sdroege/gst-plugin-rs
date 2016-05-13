use libc::{c_char};
use std::ffi::{CStr, CString};
use std::ptr;

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
pub extern "C" fn filesrc_fill(ptr: *mut FileSrc) {
    let filesrc: &mut FileSrc = unsafe { &mut *ptr };

    println!("fill {:?}", filesrc);
}

#[derive(Debug)]
pub struct FileSrc {
    location: Option<String>,
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
