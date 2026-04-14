use image::DynamicImage;

pub(crate) enum Wrapper {
    Image(DynamicImage),
}

impl AsRef<[u8]> for Wrapper {
    fn as_ref(&self) -> &[u8] {
        match self {
            Wrapper::Image(v) => v.as_bytes(),
        }
    }
}

impl Wrapper {
    pub fn into_gst_buffer(self) -> gst::Buffer {
        gst::Buffer::from_slice(self)
    }
}
