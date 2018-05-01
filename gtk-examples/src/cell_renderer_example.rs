//! # Custom Cell Renderer Sample
//!
//! This sample demonstrates how to create a gtk::CellRenderer subclass

extern crate gtk;
extern crate gio;
extern crate gdk;
#[macro_use]
extern crate glib;

extern crate cairo;
extern crate pango;
extern crate glib_sys as glib_ffi;
extern crate gobject_sys as gobject_ffi;
extern crate gtk_sys as gtk_ffi;
extern crate cairo_sys as cairo_ffi;
extern crate gdk_sys as gdk_ffi;

#[macro_use]
extern crate gobject_subclass;


use gio::prelude::*;
use gtk::prelude::*;
use gtk::{
    ApplicationWindow, Label, ListStore, Orientation, TreeView, TreeViewColumn,
    WindowPosition,
};

use std::env::args;

mod cell_renderer;
mod custom_cell_renderer;
use custom_cell_renderer::CellRendererCustom;

// make moving clones into closures more convenient
macro_rules! clone {
    (@param _) => ( _ );
    (@param $x:ident) => ( $x );
    ($($n:ident),+ => move || $body:expr) => (
        {
            $( let $n = $n.clone(); )+
            move || $body
        }
    );
    ($($n:ident),+ => move |$($p:tt),+| $body:expr) => (
        {
            $( let $n = $n.clone(); )+
            move |$(clone!(@param $p),)+| $body
        }
    );
}

fn create_and_fill_model() -> ListStore {
    // Creation of a model with two rows.
    let model = ListStore::new(&[u32::static_type(), String::static_type()]);

    // Filling up the tree view.
    let entries = &["Michel", "Sara", "Liam", "Zelda", "Neo", "Octopus master"];
    for (i, entry) in entries.iter().enumerate() {
        model.insert_with_values(None, &[0, 1], &[&(i as u32 + 1), &entry]);
    }
    model
}

fn append_column(tree: &TreeView, id: i32) {
    let column = TreeViewColumn::new();
    let cell = CellRendererCustom::new();

    column.pack_start(&cell, true);

    // Association of the view's column with the model's `id` column.
    column.add_attribute(&cell, "text", id);
    tree.append_column(&column);
}

fn create_and_setup_view() -> TreeView {
    // Creating the tree view.
    let tree = TreeView::new();

    tree.set_headers_visible(false);
    // Creating the two columns inside the view.
    append_column(&tree, 0);
    append_column(&tree, 1);
    tree
}

fn build_ui(application: &gtk::Application) {
    let window = ApplicationWindow::new(application);

    window.set_title("Simple TreeView example");
    window.set_position(WindowPosition::Center);

    window.connect_delete_event(clone!(window => move |_, _| {
        window.destroy();
        Inhibit(false)
    }));

    // Creating a vertical layout to place both tree view and label in the window.
    let vertical_layout = gtk::Box::new(Orientation::Vertical, 0);

    // Creation of the label.
    let label = Label::new(None);

    let tree = create_and_setup_view();

    let model = create_and_fill_model();
    // Setting the model into the view.
    tree.set_model(Some(&model));
    // Adding the view to the layout.
    vertical_layout.add(&tree);
    // Same goes for the label.
    vertical_layout.add(&label);

    // The closure responds to selection changes by connection to "::cursor-changed" signal,
    // that gets emitted when the cursor moves (focus changes).
    tree.connect_cursor_changed(move |tree_view| {
        let selection = tree_view.get_selection();
        if let Some((model, iter)) = selection.get_selected() {
            // Now getting back the values from the row corresponding to the
            // iterator `iter`.
            //
            // The `get_value` method do the conversion between the gtk type and Rust.
            label.set_text(&format!("Hello '{}' from row {}",
                                    model.get_value(&iter, 1)
                                         .get::<String>()
                                         .expect("Couldn't get string value"),
                                    model.get_value(&iter, 0)
                                         .get::<u32>()
                                         .expect("Couldn't get u32 value")));
        }
    });

    // Adding the layout to the window.
    window.add(&vertical_layout);

    window.show_all();
}

fn main() {
    if gtk::init().is_err() {
        panic!("Failed to initialize GTK.");
    }

    let application = gtk::Application::new("com.github.simple_treeview",
                                            gio::ApplicationFlags::empty())
                                       .expect("Initialization failed...");

    application.connect_startup(move |app| {
        build_ui(app);
    });
    application.connect_activate(|_| {});

    application.run(&args().collect::<Vec<_>>());
}
