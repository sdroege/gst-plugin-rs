import gi
import sys

gi.require_version("Gtk", "4.0")
gi.require_version('Gst', '1.0')

from gi.repository import Gtk
from gi.repository import Gst

Gst.init(sys.argv[1:])

gtksink = Gst.ElementFactory.make("gtk4paintablesink", "sink")
# Get the paintable from the sink
paintable = gtksink.props.paintable

# Use GL if available
if paintable.props.gl_context:
    print("Using GL")
    source = Gst.ElementFactory.make("gltestsrc", "source")
    glsink = Gst.ElementFactory.make("glsinkbin", "sink")
    glsink.props.sink = gtksink
    sink = glsink
else:
    source = Gst.ElementFactory.make("videotestsrc", "source")
    sink = gtksink

pipeline = Gst.Pipeline.new()

if not pipeline or not source or not sink:
    print("Not all elements could be created.")
    exit(-1)

pipeline.add(source)
pipeline.add(sink)
source.link(sink)

def on_activate(app):
    win = Gtk.ApplicationWindow(application=app)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)

    picture = Gtk.Picture.new()
    picture.set_size_request(640, 480)
    # Set the paintable on the picture
    picture.set_paintable(paintable)
    box.append(picture)

    btn = Gtk.Button(label="▶/⏸")
    box.append(btn)
    btn.connect('clicked', lambda _: pipeline.set_state(Gst.State.PAUSED) if pipeline.get_state(1)[1]==Gst.State.PLAYING else pipeline.set_state(Gst.State.PLAYING))

    win.set_child(box)
    win.present()

app = Gtk.Application()
app.connect('activate', on_activate)

app.run(None)
