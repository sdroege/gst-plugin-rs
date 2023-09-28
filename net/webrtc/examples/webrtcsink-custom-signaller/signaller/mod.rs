use gst::glib;
use gstrswebrtc::signaller::Signallable;

mod imp;

glib::wrapper! {
    pub struct MyCustomSignaller(ObjectSubclass<imp::Signaller>) @implements Signallable;
}

unsafe impl Send for MyCustomSignaller {}
unsafe impl Sync for MyCustomSignaller {}

impl MyCustomSignaller {
    pub fn new() -> Self {
        glib::Object::new()
    }
}

impl Default for MyCustomSignaller {
    fn default() -> Self {
        MyCustomSignaller::new()
    }
}
