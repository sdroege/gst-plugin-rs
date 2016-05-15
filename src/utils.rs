#[macro_export]
macro_rules! println_err(
    ($($arg:tt)*) => { {
        let r = writeln!(&mut ::std::io::stderr(), $($arg)*);
        // Ignore when writing fails
        match r {
            Ok(_) => (),
            Err(_) => ()
        };
    } }
);

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

impl GBoolean {
    pub fn from_bool(v: bool) -> GBoolean {
        match v {
            true => GBoolean::True,
            false => GBoolean::False,
        }
    }
}

