
use std::sync::{
    Mutex,
    Once,
    ONCE_INIT
};

use glib;
use gtk;
use gtk::prelude::*;

use gobject_subclass::object::*;
use gobject_subclass::properties::*;

use cell_renderer::*;


pub trait CellRendererCustomImpl: 'static {
    // fn uri_validator(&self) -> Box<UriValidator>;
    //
    // fn start(&mut self, renderer: &CellRenderer, uri: Url) -> Result<(), gst::ErrorMessage>;
    // fn stop(&mut self, renderer: &CellRenderer) -> Result<(), gst::ErrorMessage>;
    // fn render(&mut self, renderer: &CellRenderer, buffer: &gst::BufferRef) -> Result<(), FlowError>;
}

pub struct CellRendererCustom {
    // cat: gst::DebugCategory,
    // uri: Mutex<(Option<Url>, bool)>,
    // uri_validator: Box<UriValidator>,
    //imp: Mutex<Box<CellRendererCustomImpl>>,
}

static PROPERTIES: [Property; 0] = [
];

impl CellRendererCustom {
    pub fn new() -> Self {

        static ONCE: Once = ONCE_INIT;
        ONCE.call_once(|| {
            let static_instance = CellRendererCustomStatic::default();
            register_type(static_instance);
        });
        // let renderer_impl = (renderer_info.create_instance)(renderer);

        // Self {
        //
        //     // imp: Mutex::new(renderer_impl),
        // }
    }

    //     pub fn new() -> CellRendererThread {
    //         println!("CellRendererThread::new");
    //         unsafe { from_glib_full(cell_renderer_thread_new()) }
    //     }

    fn class_init(klass: &mut CellRendererClass) {


        klass.install_properties(&PROPERTIES);
    }

    fn init(renderer: &CellRenderer)// -> Box<CellRendererImpl<CellRenderer>>
    {
        //
        // let imp = Self::new(renderer);
        // Box::new(imp)
    }
}

impl ObjectImpl<CellRenderer> for CellRendererCustom{




    fn set_property(&self, obj: &glib::Object, id: u32, value: &glib::Value) {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            _ => unimplemented!(),
        }
    }

    fn get_property(&self, obj: &glib::Object, id: u32) -> Result<glib::Value, ()> {
        let prop = &PROPERTIES[id as usize];

        match *prop {
            _ => unimplemented!(),
        }
    }
}

impl CellRendererImpl<CellRenderer> for CellRendererCustom {


    // fn render(&self, renderer: &CellRenderer, buffer: &gst::BufferRef) -> gst::FlowReturn {
    //     let renderer_impl = &mut self.imp.lock().unwrap();
    //
    //     gst_trace!(self.cat, obj: renderer, "Rendering buffer {:?}", buffer,);
    //
    //     match renderer_impl.render(renderer, buffer) {
    //         Ok(()) => gst::FlowReturn::Ok,
    //         Err(flow_error) => {
    //             gst_error!(self.cat, obj: renderer, "Failed to render: {:?}", flow_error);
    //             match flow_error {
    //                 FlowError::NotNegotiated(ref msg) | FlowError::Error(ref msg) => {
    //                     renderer.post_error_message(msg);
    //                 }
    //                 _ => (),
    //             }
    //             flow_error.into()
    //         }
    //     }
    // }
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


// pub fn renderer_register(plugin: &gst::Plugin, renderer_info: CellRendererCustomInfo) {
//     let name = renderer_info.name.clone();
//     let rank = renderer_info.rank;
//
//     let renderer_static = CellRendererCustomStatic {
//         name: format!("CellRendererCustom-{}", name),
//         renderer_info: renderer_info,
//     };
//
//     let type_ = register_type(renderer_static);
//     gst::Element::register(plugin, &name, rank, type_);
// }
