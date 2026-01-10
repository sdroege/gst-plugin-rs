macro_rules! debug_or_trace {
    ($cat:expr_2021, $raise_log_level:expr_2021, $qual:ident = $obj:expr_2021, $rest:tt $(,)?) => {
        if $raise_log_level {
            gst::debug!($cat, $qual = $obj, $rest);
        } else {
            gst::trace!($cat, $qual = $obj, $rest);
        }
    };
}

macro_rules! log_or_trace {
    ($cat:expr_2021, $raise_log_level:expr_2021, $qual:ident = $obj:expr_2021, $rest:tt $(,)?) => {
        if $raise_log_level {
            gst::log!($cat, $qual = $obj, $rest);
        } else {
            gst::trace!($cat, $qual = $obj, $rest);
        }
    };
}
