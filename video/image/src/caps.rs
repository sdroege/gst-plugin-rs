#[allow(unused_macros)]
macro_rules! append_caps {
    ($c:ident, $mime:literal) => (
        $c.append(gst::Caps::new_empty_simple($mime));
    );
    ($c:ident, $mime:literal, $($mime2:literal),+) => (
        $c.append(gst::Caps::new_empty_simple($mime));
        append_caps!($c, $($mime2),+)
    );
}

#[allow(unused_macros)]
macro_rules! make_caps {
    ($mime:literal) => {
        gst::Caps::new_empty_simple($mime)
    };

    ($($mime:literal),+) => {
        {
            let mut caps = gst::Caps::new_empty();
            let c = caps.make_mut();
            append_caps!(c, $($mime),+);
            caps
        }
    };

    ($format:expr) => {
        gst::Caps::new_empty_simple($format.to_mime_type())
    };
}

#[allow(unused_macros)]
macro_rules! make_caps_with_extra_mimetypes {
    ($format:expr, $($mime:literal),+) => {
        {
            let mut caps = gst::Caps::new_empty();
            let c = caps.make_mut();
            c.append(gst::Caps::new_empty_simple($format.to_mime_type()));
            append_caps!(c, $($mime),+);
            caps
        }
    };
}

#[allow(unused_macros)]
macro_rules! make_encoder_caps {
    ($mime:literal, $formats:expr) => {{
        let caps = gst::Caps::new_empty_simple($mime);
        (caps, Vec::from_iter($formats))
    }};

    ($format:expr, $formats:expr) => {{
        let caps = gst::Caps::new_empty_simple($format.to_mime_type());
        (caps, Vec::from_iter($formats))
    }};
}

#[allow(unused_macros)]
macro_rules! make_encoder_caps_with_extra_mimetypes {
    ($format:expr, $($mime:literal),+, $formats:expr) => {{
        let mut caps = gst::Caps::new_empty();
        let c = caps.make_mut();
        c.append(gst::Caps::new_empty_simple($format.to_mime_type()));
        append_caps!(c, $($mime),+);
        (caps, Vec::from_iter($formats))
    }};
}
