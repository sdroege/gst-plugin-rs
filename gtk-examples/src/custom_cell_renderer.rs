
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
    // fn start(&mut self, sink: &CellRenderer, uri: Url) -> Result<(), gst::ErrorMessage>;
    // fn stop(&mut self, sink: &CellRenderer) -> Result<(), gst::ErrorMessage>;
    // fn render(&mut self, sink: &CellRenderer, buffer: &gst::BufferRef) -> Result<(), FlowError>;
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
    fn new(sink: &CellRenderer, sink_info: &CellRendererCustomInfo) -> Self {
        let sink_impl = (sink_info.create_instance)(sink);

        Self {

            imp: Mutex::new(sink_impl),
        }
    }

    fn class_init(klass: &mut CellRendererClass, sink_info: &CellRendererCustomInfo) {


        klass.install_properties(&PROPERTIES);
    }

    fn init(element: &CellRenderer, sink_info: &SinkInfo) -> Box<CellRendererImpl<CellRenderer>> {

        let imp = Self::new(element, sink_info);
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
    // fn start(&self, sink: &CellRenderer) -> bool {
    //     gst_debug!(self.cat, obj: sink, "Starting");
    //
    //     // Don't keep the URI locked while we call start later
    //     let uri = match *self.uri.lock().unwrap() {
    //         (Some(ref uri), ref mut started) => {
    //             *started = true;
    //             uri.clone()
    //         }
    //         (None, _) => {
    //             gst_error!(self.cat, obj: sink, "No URI given");
    //             gst_element_error!(sink, gst::ResourceError::OpenRead, ["No URI given"]);
    //             return false;
    //         }
    //     };
    //
    //     let sink_impl = &mut self.imp.lock().unwrap();
    //     match sink_impl.start(sink, uri) {
    //         Ok(..) => {
    //             gst_trace!(self.cat, obj: sink, "Started successfully");
    //             true
    //         }
    //         Err(ref msg) => {
    //             gst_error!(self.cat, obj: sink, "Failed to start: {:?}", msg);
    //
    //             self.uri.lock().unwrap().1 = false;
    //             sink.post_error_message(msg);
    //             false
    //         }
    //     }
    // }
    //
    // fn stop(&self, sink: &CellRenderer) -> bool {
    //     let sink_impl = &mut self.imp.lock().unwrap();
    //
    //     gst_debug!(self.cat, obj: sink, "Stopping");
    //
    //     match sink_impl.stop(sink) {
    //         Ok(..) => {
    //             gst_trace!(self.cat, obj: sink, "Stopped successfully");
    //             self.uri.lock().unwrap().1 = false;
    //             true
    //         }
    //         Err(ref msg) => {
    //             gst_error!(self.cat, obj: sink, "Failed to stop: {:?}", msg);
    //
    //             sink.post_error_message(msg);
    //             false
    //         }
    //     }
    // }
    //
    // fn render(&self, sink: &CellRenderer, buffer: &gst::BufferRef) -> gst::FlowReturn {
    //     let sink_impl = &mut self.imp.lock().unwrap();
    //
    //     gst_trace!(self.cat, obj: sink, "Rendering buffer {:?}", buffer,);
    //
    //     match sink_impl.render(sink, buffer) {
    //         Ok(()) => gst::FlowReturn::Ok,
    //         Err(flow_error) => {
    //             gst_error!(self.cat, obj: sink, "Failed to render: {:?}", flow_error);
    //             match flow_error {
    //                 FlowError::NotNegotiated(ref msg) | FlowError::Error(ref msg) => {
    //                     sink.post_error_message(msg);
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
    sink_info: SinkInfo,
}

impl ImplTypeStatic<CellRenderer> for CellRendererCustomStatic {
    fn get_name(&self) -> &str {
        self.name.as_str()
    }

    fn new(&self, element: &CellRenderer) -> Box<CellRendererImpl<CellRenderer>> {
        CellRendererCustom::init(element, &self.sink_info)
    }

    fn class_init(&self, klass: &mut CellRendererClass) {
        CellRendererCustom::class_init(klass, &self.sink_info);
    }

    fn type_init(&self, token: &TypeInitToken, type_: glib::Type) {

    }
}


// pub fn sink_register(plugin: &gst::Plugin, sink_info: SinkInfo) {
//     let name = sink_info.name.clone();
//     let rank = sink_info.rank;
//
//     let sink_static = SinkStatic {
//         name: format!("Sink-{}", name),
//         sink_info: sink_info,
//     };
//
//     let type_ = register_type(sink_static);
//     gst::Element::register(plugin, &name, rank, type_);
// }
