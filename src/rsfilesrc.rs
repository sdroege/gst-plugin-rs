use std::mem;

#[no_mangle]
pub extern "C" fn filesrc_new() -> *mut FileSrc {
    let mut instance = Box::new(FileSrc::new());
    return &mut *instance;
}

#[no_mangle]
pub extern "C" fn filesrc_drop(ptr: *mut FileSrc) {
    let filesrc: &mut FileSrc = unsafe { &mut *ptr };

    println!("drop");
    drop(filesrc);
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
