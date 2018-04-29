use std::rc::Rc;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::ptr;
use std::mem;
use std::ffi::CString;

use gio;
use glib;
use gtk;
use gdk;
use cairo;
use glib::prelude::*;

use glib::translate::*;
use gtk::prelude::*;
use glib_ffi;
use gobject_ffi;
use cairo_ffi;
use gtk_ffi;
use gdk_ffi;
use glib::object::Downcast;
use glib::IsA;

use gobject_subclass::object::*;
use gobject_subclass::anyimpl::*;
use gobject_subclass::properties::*;




pub trait CellRendererImpl<T: CellRendererBase>: ObjectImpl<T> + AnyImpl + 'static
{

    // fn new(){
    //
    // }

    fn render(&self, cell_renderer: &T,
                     cr: &cairo::Context,
                     widget: &gtk::Widget,
                     background_area: &gtk::Rectangle,
                     cell_area: &gtk::Rectangle,
                     flags: gtk::CellRendererState)
    {
        cell_renderer.parent_render(cr, widget, background_area, cell_area, flags)
    }
}

pub trait CellRendererImplExt<T> {


    // fn catch_panic_pad_function<R, F: FnOnce(&Self, &T) -> R, G: FnOnce() -> R>(
    //     parent: &Option<gtk::Object>,
    //     fallback: G,
    //     f: F,
    // ) -> R;
}

impl<S: CellRendererImpl<T>, T: ObjectType + glib::IsA<gtk::CellRenderer>>
    CellRendererImplExt<T> for S
{


    // fn catch_panic_pad_function<R, F: FnOnce(&Self, &T) -> R, G: FnOnce() -> R>(
    //     parent: &Option<gtk::Object>,
    //     fallback: G,
    //     f: F,
    // ) -> R {
    //     // FIXME: Does this work for cell_renderer subclasses?
    //     let cell_renderer = parent.as_ref().cloned().unwrap().downcast::<T>().unwrap();
    //     let imp = cell_renderer.get_impl();
    //     let imp = Any::downcast_ref::<Box<CellRendererImpl<T> + 'static>>(imp).unwrap();
    //     let imp = imp.downcast_ref::<S>().unwrap();
    //     cell_renderer.catch_panic(fallback, |cell_renderer| f(imp, cell_renderer))
    // }
}

any_impl!(CellRendererBase, CellRendererImpl);

pub unsafe trait CellRendererBase: IsA<gtk::CellRenderer> + ObjectType
{

    fn parent_render(&self, cr: &cairo::Context,
                            widget: &gtk::Widget,
                            background_area: &gtk::Rectangle,
                            cell_area: &gtk::Rectangle,
                            flags: gtk::CellRendererState)
    {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gtk_ffi::GtkCellRendererClass;
            (*parent_klass)
                .render
                .map(|f| f(self.to_glib_none().0,
                           cr.to_glib_none().0,
                           widget.to_glib_none().0,
                           background_area.to_glib_none().0,
                           cell_area.to_glib_none().0,
                           flags.to_glib()))
                .unwrap_or(())
        }
    }

}

pub unsafe trait CellRendererClassExt<T: CellRendererBase>
where
    T::ImplType: CellRendererImpl<T>
{

    fn override_vfuncs(&mut self, _: &ClassInitToken)
    {
        unsafe {
            let klass = &mut *(self as *const Self as *mut gtk_ffi::GtkCellRendererClass);
            klass.render = Some(cell_renderer_render::<T>);
        }
    }
}

glib_wrapper! {
    pub struct CellRenderer(Object<InstanceStruct<CellRenderer>>):
        [gtk::CellRenderer => gtk_ffi::GtkCellRenderer];

    match fn {
        get_type => || get_type::<CellRenderer>(),
    }
}

unsafe impl<T: IsA<gtk::CellRenderer> + ObjectType> CellRendererBase for T{}

pub type CellRendererClass = ClassStruct<CellRenderer>;

// FIXME: Boilerplate
unsafe impl CellRendererClassExt<CellRenderer> for CellRendererClass {}


#[macro_export]
macro_rules! box_cell_renderer_impl(
    ($name:ident) => {
        box_object_impl!($name);

        impl<T: CellRendererBase> CellRendererImpl<T> for Box<$name<T>>
        {
            fn render(&self, cell_renderer: &T,
                             cr: &cairo::Context,
                             widget: &gtk::Widget,
                             background_area: &gtk::Rectangle,
                             cell_area: &gtk::Rectangle,
                             flags: gtk::CellRendererState)
            {
                let imp: &$name<T> = self.as_ref();
                imp.render(cell_renderer, cr, widget, background_area, cell_area, flags)
            }
        }
    };
);

box_cell_renderer_impl!(CellRendererImpl);

impl ObjectType for CellRenderer
{
    const NAME: &'static str = "RsCellRenderer";
    type GlibType = gtk_ffi::GtkCellRenderer;
    type GlibClassType = gtk_ffi::GtkCellRendererClass;
    type ImplType = Box<CellRendererImpl<Self>>;
    type InstanceStructType = InstanceStruct<Self>;

    fn glib_type() -> glib::Type {
        unsafe { from_glib(gtk_ffi::gtk_cell_renderer_get_type()) }
    }

    fn class_init(token: &ClassInitToken, klass: &mut CellRendererClass) {
        klass.override_vfuncs(token);
    }

    object_type_fns!();
}


// This will create a new C type. But where do I put the ::new()?

#[no_mangle]
pub unsafe extern "C" fn cell_renderer_thread_new<T: CellRendererBase>()
     -> *mut T::InstanceStructType
where
    T::ImplType: CellRendererImpl<T>
{
    callback_guard!();
    let this = gobject_ffi::g_object_newv(T::glib_type().to_glib(), 0, ptr::null_mut());
    this as *mut T::InstanceStructType
}

#[no_mangle]
unsafe extern "C" fn cell_renderer_render<T: CellRendererBase>(
    ptr: *mut gtk_ffi::GtkCellRenderer,
    cr: *mut cairo_ffi::cairo_t,
    widget: *mut gtk_ffi::GtkWidget,
    background_area: *const gdk_ffi::GdkRectangle,
    cell_area: *const gdk_ffi::GdkRectangle,
    flags: gtk_ffi::GtkCellRendererState
)where
    T::ImplType: CellRendererImpl<T>
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let cell_renderer = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = cell_renderer.get_impl();

    imp.render(&wrap, &from_glib_borrow(cr),
                      &from_glib_borrow(widget),
                      &from_glib_borrow(background_area),
                      &from_glib_borrow(cell_area),
                      from_glib(flags))
}
