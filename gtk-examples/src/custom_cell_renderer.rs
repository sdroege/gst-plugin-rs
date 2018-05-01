
use std::sync::{
    Mutex,
    Once,
    ONCE_INIT
};
use std::cell::RefCell;

use glib;
use glib::prelude::*;
use gtk;
use gtk::prelude::*;
use cairo;
use pango;

use gobject_subclass::object::*;
use gobject_subclass::properties::*;

use cell_renderer::*;


pub trait CellRendererCustomImpl: 'static {
}


pub struct CellRendererCustom {
    text: RefCell<String>
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
            text: RefCell::new("".to_string())
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
                self.text.replace(text);
            },
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            Property::String("text", ..) => {
                Ok(self.text.borrow().clone().to_value())
            },
            _ => unimplemented!(),
        }
    }
}

impl CellRendererImpl<CellRenderer> for CellRendererCustom {

    fn render(&self,
        renderer: &CellRenderer,
        cr: &cairo::Context,
        widget: &gtk::Widget,
        background_area: &gtk::Rectangle,
        cell_area: &gtk::Rectangle,
        flags: gtk::CellRendererState,
    ){

        let layout = widget.create_pango_layout(self.text.borrow().as_str()).unwrap();
        let sc = widget.get_style_context().unwrap();
        let (padx, pady) = renderer.get_padding();

        cr.save();
        cr.rectangle(cell_area.x.into(), cell_area.y.into(), cell_area.width.into(), cell_area.height.into());
        cr.clip();

        gtk::render_layout(&sc, cr, (cell_area.x + padx).into(), (cell_area.y + pady).into(), &layout);

        cr.restore();
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
