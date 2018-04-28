
use std::sync::Mutex;

use glib;
use gtk;
use gtk::prelude::*;

use gobject_subclass::object::*;
use gobject_subclass::properties::*;

use cell_renderer::*;


pub trait CellRendererCustomImpl: Send + 'static {
    // fn uri_validator(&self) -> Box<UriValidator>;
    //
    // fn start(&mut self, renderer: &CellRenderer, uri: Url) -> Result<(), gst::ErrorMessage>;
    // fn stop(&mut self, renderer: &CellRenderer) -> Result<(), gst::ErrorMessage>;
    // fn render(&mut self, renderer: &CellRenderer, buffer: &gst::BufferRef) -> Result<(), FlowError>;
}

struct CellRendererCustom {
    // cat: gst::DebugCategory,
    // uri: Mutex<(Option<Url>, bool)>,
    // uri_validator: Box<UriValidator>,
    imp: Mutex<Box<CellRendererCustomImpl>>,
}

static PROPERTIES: [Property; 0] = [
];

impl CellRendererCustom {
    fn new(renderer: &CellRenderer, renderer_info: &CellRendererCustomInfo) -> Self {
        let renderer_impl = (renderer_info.create_instance)(renderer);

        Self {

            imp: Mutex::new(renderer_impl),
        }
    }

    fn class_init(klass: &mut CellRendererClass, renderer_info: &CellRendererCustomInfo) {


        klass.install_properties(&PROPERTIES);
    }

    fn init(element: &CellRenderer, renderer_info: &CellRendererCustomInfo) -> Box<CellRendererImpl<CellRenderer>> {

        let imp = Self::new(element, renderer_info);
        Box::new(imp)
    }


}

impl ObjectImpl<CellRenderer> for CellRendererCustom {
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
    // fn start(&self, renderer: &CellRenderer) -> bool {
    //     gst_debug!(self.cat, obj: renderer, "Starting");
    //
    //     // Don't keep the URI locked while we call start later
    //     let uri = match *self.uri.lock().unwrap() {
    //         (Some(ref uri), ref mut started) => {
    //             *started = true;
    //             uri.clone()
    //         }
    //         (None, _) => {
    //             gst_error!(self.cat, obj: renderer, "No URI given");
    //             gst_element_error!(renderer, gst::ResourceError::OpenRead, ["No URI given"]);
    //             return false;
    //         }
    //     };
    //
    //     let renderer_impl = &mut self.imp.lock().unwrap();
    //     match renderer_impl.start(renderer, uri) {
    //         Ok(..) => {
    //             gst_trace!(self.cat, obj: renderer, "Started successfully");
    //             true
    //         }
    //         Err(ref msg) => {
    //             gst_error!(self.cat, obj: renderer, "Failed to start: {:?}", msg);
    //
    //             self.uri.lock().unwrap().1 = false;
    //             renderer.post_error_message(msg);
    //             false
    //         }
    //     }
    // }
    //
    // fn stop(&self, renderer: &CellRenderer) -> bool {
    //     let renderer_impl = &mut self.imp.lock().unwrap();
    //
    //     gst_debug!(self.cat, obj: renderer, "Stopping");
    //
    //     match renderer_impl.stop(renderer) {
    //         Ok(..) => {
    //             gst_trace!(self.cat, obj: renderer, "Stopped successfully");
    //             self.uri.lock().unwrap().1 = false;
    //             true
    //         }
    //         Err(ref msg) => {
    //             gst_error!(self.cat, obj: renderer, "Failed to stop: {:?}", msg);
    //
    //             renderer.post_error_message(msg);
    //             false
    //         }
    //     }
    // }
    //
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

pub struct CellRendererCustomInfo {
    pub name: String,
    pub long_name: String,
    pub description: String,
    pub classification: String,
    pub author: String,
    pub rank: u32,
    pub create_instance: fn(&CellRenderer) -> Box<CellRendererCustomImpl>,
    pub protocols: Vec<String>,
}

struct CellRendererCustomStatic {
    name: String,
    renderer_info: CellRendererCustomInfo,
}

impl ImplTypeStatic<CellRenderer> for CellRendererCustomStatic {
    fn get_name(&self) -> &str {
        self.name.as_str()
    }

    fn new(&self, element: &CellRenderer) -> Box<CellRendererImpl<CellRenderer>> {
        CellRendererCustom::init(element, &self.renderer_info)
    }

    fn class_init(&self, klass: &mut CellRendererClass) {
        CellRendererCustom::class_init(klass, &self.renderer_info);
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
