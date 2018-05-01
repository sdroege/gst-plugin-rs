
use std::sync::{
    Mutex,
    Once,
    ONCE_INIT
};
use std::cell::Cell;

use glib;
use glib::prelude::*;
use gtk;
use gtk::prelude::*;
use cairo;

use gobject_subclass::object::*;
use gobject_subclass::properties::*;

use cell_renderer::*;


pub trait CellRendererCustomImpl: 'static {
}


pub struct CellRendererCustom {
    text: Cell<Option<String>>
}

static PROPERTIES: [Property; 1] = [
    Property::String(
        "text",
        "Text",
        "Text to render",
        None,
        PropertyMutability::ReadWrite,
    ),
];

impl CellRendererCustom {
    pub fn new() -> CellRenderer {
        use glib::object::Downcast;

        static ONCE: Once = ONCE_INIT;
        static mut TYPE: glib::Type = glib::Type::Invalid;

        ONCE.call_once(|| {
            let static_instance = CellRendererCustomStatic::default();
            let t = register_type(static_instance);
            unsafe {
                TYPE = t;
            }
        });

        unsafe {
            glib::Object::new(TYPE, &[]).unwrap().downcast_unchecked()
        }
    }


    fn class_init(klass: &mut CellRendererClass) {


        klass.install_properties(&PROPERTIES);
    }

    fn init(_renderer: &CellRenderer) -> Box<CellRendererImpl<CellRenderer>>
    {
        let imp = Self {
            text: Cell::new(None)
        };
        Box::new(imp)
    }
}

impl ObjectImpl<CellRenderer> for CellRendererCustom{


    fn set_property(&self, obj: &glib::Object, id: u32, value: &glib::Value) {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::String("text", ..) => {
                let text: String = value.get().unwrap();
                self.text.set(Some(text));
            },
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            // Property::String("text", ..) => {
            //     self.text.unwrap_or("".to_string())
            // },
            _ => unimplemented!(),
        }
    }
}

impl CellRendererImpl<CellRenderer> for CellRendererCustom {

    fn render(&self,
        _renderer: &CellRenderer,
        cr: &cairo::Context,
        widget: &gtk::Widget,
        background_area: &gtk::Rectangle,
        cell_area: &gtk::Rectangle,
        flags: gtk::CellRendererState,
    ){
        print!("waaah",);
    }


}

#[derive(Default)]
pub struct CellRendererCustomStatic{

}

impl ImplTypeStatic<CellRenderer> for CellRendererCustomStatic {
    fn get_name(&self) -> &str {
        "CellRendererCustom"
    }

    fn new(&self, renderer: &CellRenderer) -> Box<CellRendererImpl<CellRenderer>> {
        CellRendererCustom::init(renderer)
    }

    fn class_init(&self, klass: &mut CellRendererClass) {
        CellRendererCustom::class_init(klass);
    }

    fn type_init(&self, token: &TypeInitToken, type_: glib::Type) {

    }
}
