use glib::{gobject_sys, StaticType, Type};

// This enum may be used to control what type of output the progressbin should produce.
// It also serves the secondary purpose of illustrating how to add enum-type properties
// to a plugin written in rust.
#[derive(Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Clone, Copy)]
#[repr(u32)]
pub(crate) enum ProgressBinOutput {
    Println = 0,
    DebugCategory = 1,
}

// This trait allows us to translate our custom enum type to a GLib type.
impl glib::translate::ToGlib for ProgressBinOutput {
    // We use i32 as the underlying representation for the enum.
    type GlibType = i32;

    fn to_glib(&self) -> Self::GlibType {
        *self as Self::GlibType
    }
}

// This trait allows us to translate from a GLib type back to our custom enum type.
impl glib::translate::FromGlib<i32> for ProgressBinOutput {
    fn from_glib(val: i32) -> Self {
        match val {
            0 => ProgressBinOutput::Println,
            1 => ProgressBinOutput::DebugCategory,
            _ => unreachable!(),
        }
    }
}

// This trait registers our enum with the GLib type system.
impl StaticType for ProgressBinOutput {
    fn static_type() -> Type {
        progressbin_output_get_type()
    }
}

impl<'a> glib::value::FromValueOptional<'a> for ProgressBinOutput {
    unsafe fn from_value_optional(value: &glib::Value) -> Option<Self> {
        Some(glib::value::FromValue::from_value(value))
    }
}
impl<'a> glib::value::FromValue<'a> for ProgressBinOutput {
    unsafe fn from_value(value: &glib::Value) -> Self {
        use glib::translate::ToGlibPtr;

        glib::translate::from_glib(gobject_sys::g_value_get_enum(value.to_glib_none().0))
    }
}

impl glib::value::SetValue for ProgressBinOutput {
    unsafe fn set_value(value: &mut glib::Value, this: &Self) {
        use glib::translate::{ToGlib, ToGlibPtrMut};

        gobject_sys::g_value_set_enum(value.to_glib_none_mut().0, this.to_glib())
    }
}

// On the first call this function will register the enum type with GObject,
// on all further calls it will simply return the registered type id.
fn progressbin_output_get_type() -> glib::Type {
    use std::sync::Once;
    // We only want to register the enum once, and hence we use a Once struct to ensure that.
    static ONCE: Once = Once::new();
    static mut TYPE: glib::Type = glib::Type::Invalid;

    // The first time anyone calls this function, we will call ONCE to complete the registration
    // process, otherwise we'll just skip it
    ONCE.call_once(|| {
        use std::ffi;
        use std::ptr;

        // The descriptions in this array will be available to users of the plugin when using gst-inspect-1.0
        static mut VALUES: [gobject_sys::GEnumValue; 3] = [
            gobject_sys::GEnumValue {
                value: ProgressBinOutput::Println as i32,
                value_name: b"Println: Outputs the progress using a println! macro.\0".as_ptr() as *const _,
                value_nick: b"println\0".as_ptr() as *const _,
            },
            gobject_sys::GEnumValue {
                value: ProgressBinOutput::DebugCategory as i32,
                value_name: b"Debug Category: Outputs the progress as info logs under the element's debug category.\0".as_ptr() as *const _,
                value_nick: b"debug-category\0".as_ptr() as *const _,
            },
            // We use a struct with all null values to indicate the end of the array
            gobject_sys::GEnumValue {
                value: 0,
                value_name: ptr::null(),
                value_nick: ptr::null(),
            },
        ];

        // Here we call the bindings to register the enum. We store the result in TYPE, so that we
        // may re-use it later
        let name = ffi::CString::new("GstProgressBinOutput").unwrap();
        unsafe {
            let type_ = gobject_sys::g_enum_register_static(name.as_ptr(), VALUES.as_ptr());
            TYPE = glib::translate::from_glib(type_);
        }
    });

    // Check that TYPE is not invalid and return it if so
    unsafe {
        assert_ne!(TYPE, glib::Type::Invalid);
        TYPE
    }
}
