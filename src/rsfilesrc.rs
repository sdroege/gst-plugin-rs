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
pub extern "C" fn filesrc_fill(ptr: *mut FileSrc) {
    let filesrc: &mut FileSrc = unsafe { &mut *ptr };

    println!("fill");
    filesrc.location = Some(String::from("bla"));
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
