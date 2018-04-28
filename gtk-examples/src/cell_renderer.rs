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




pub trait CellRendererImpl<T: CellRendererBase>: ObjectImpl<T> + AnyImpl + Send + Sync + 'static
{
    fn set_context(&self, cell_renderer: &T, context: &gtk::IMContext) {
        cell_renderer.parent_set_context(context)
    }
}

pub trait CellRendererImplExt<T> {
    // fn catch_panic_pad_function<R, F: FnOnce(&Self, &T) -> R, G: FnOnce() -> R>(
    //     parent: &Option<gtk::Object>,
    //     fallback: G,
    //     f: F,
    // ) -> R;
}

impl<S: CellRendererImpl<T>, T: ObjectType + glib::IsA<gtk::CellRenderer> + glib::IsA<gtk::Object>>
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

    fn parent_set_context(&self, context: &gtk::IMContext) {
        unsafe {
            let klass = self.get_class();
            let parent_klass = (*klass).get_parent_class() as *const gtk_ffi::GtkCellRendererClass;
            (*parent_klass)
                .set_context
                .map(|f| f(self.to_glib_none().0, context.to_glib_none().0))
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
            klass.set_context = Some(cell_renderer_set_context::<T>);
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

unsafe impl Send for CellRenderer {}
unsafe impl Sync for CellRenderer {}

#[macro_export]
macro_rules! box_cell_renderer_impl(
    ($name:ident) => {
        box_object_impl!($name);

        impl<T: CellRendererBase> CellRendererImpl<T> for Box<$name<T>>
        {
            fn set_context(&self, cell_renderer: &T, context: &gtk::IMContext) {
                let imp: &$name<T> = self.as_ref();
                imp.set_context(cell_renderer, context)
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


unsafe extern "C" fn cell_renderer_set_context<T: CellRendererBase>(
    ptr: *mut gtk_ffi::GtkCellRenderer,
    context: *mut gtk_ffi::GtkIMContext,
)where
    T::ImplType: CellRendererImpl<T>
{
    callback_guard!();
    floating_reference_guard!(ptr);
    let cell_renderer = &*(ptr as *mut T::InstanceStructType);
    let wrap: T = from_glib_borrow(ptr as *mut T::InstanceStructType);
    let imp = cell_renderer.get_impl();

    imp.set_context(&wrap, &from_glib_borrow(context))
}
