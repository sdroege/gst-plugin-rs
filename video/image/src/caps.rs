#[allow(unused_macros)]
macro_rules! append_caps {
    ($c:ident, $mime:literal) => (
        $c.append(gst::Caps::builder($mime).build());
    );
    ($c:ident, $mime:literal, $($mime2:literal),+) => (
        $c.append(gst::Caps::builder($mime).build());
        append_caps!($c, $($mime2),+)
    );
}

#[allow(unused_macros)]
macro_rules! make_caps {
    ($mime:literal) => {
        gst::Caps::builder($mime).build()
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
        gst::Caps::builder($format.to_mime_type()).build()
    };
}

#[allow(unused_macros)]
macro_rules! make_caps_with_extra_mimetypes {
    ($format:expr, $($mime:literal),+) => {
        {
            let mut caps = gst::Caps::new_empty();
            let c = caps.make_mut();
            c.append(gst::Caps::builder($format.to_mime_type()).build());
            append_caps!(c, $($mime),+);
            caps
        }
    };
}
