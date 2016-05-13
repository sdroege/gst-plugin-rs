#include "rsfilesink.h"

GST_DEBUG_CATEGORY_STATIC (gst_rsfile_sink_debug);
#define GST_CAT_DEFAULT gst_rsfile_sink_debug

static GstStaticPadTemplate sink_template = GST_STATIC_PAD_TEMPLATE ("sink",
    GST_PAD_SINK,
    GST_PAD_ALWAYS,
    GST_STATIC_CAPS_ANY);

enum
{
  PROP_0,
  PROP_LOCATION
};

static void gst_rsfile_sink_finalize (GObject * object);

static void gst_rsfile_sink_set_property (GObject * object, guint prop_id,
    const GValue * value, GParamSpec * pspec);
static void gst_rsfile_sink_get_property (GObject * object, guint prop_id,
    GValue * value, GParamSpec * pspec);

static gboolean gst_rsfile_sink_start (GstBaseSink * basesink);
static gboolean gst_rsfile_sink_stop (GstBaseSink * basesink);
static gboolean gst_rsfile_sink_query (GstBaseSink * basesink, GstQuery * query);
static GstFlowReturn gst_rsfile_sink_render (GstBaseSink * sink, GstBuffer * buffer);
static gboolean gst_rsfile_sink_event (GstBaseSink * sink, GstEvent * event);

#define _do_init \
  GST_DEBUG_CATEGORY_INIT (gst_rsfile_sink_debug, "rsfilesink", 0, "rsfilesink element");
#define gst_rsfile_sink_parent_class parent_class
G_DEFINE_TYPE_WITH_CODE (GstRsFileSink, gst_rsfile_sink, GST_TYPE_BASE_SINK, _do_init);

static void
gst_rsfile_sink_class_init (GstRsFileSinkClass * klass)
{
  GObjectClass *gobject_class;
  GstElementClass *gstelement_class;
  GstBaseSinkClass *gstbasesink_class;

  gobject_class = G_OBJECT_CLASS (klass);
  gstelement_class = GST_ELEMENT_CLASS (klass);
  gstbasesink_class = GST_BASE_SINK_CLASS (klass);

  gobject_class->set_property = gst_rsfile_sink_set_property;
  gobject_class->get_property = gst_rsfile_sink_get_property;

  g_object_class_install_property (gobject_class, PROP_LOCATION,
      g_param_spec_string ("location", "File Location",
          "Location of the file to read", NULL,
          G_PARAM_READWRITE | G_PARAM_STATIC_STRINGS |
          GST_PARAM_MUTABLE_READY));

  gobject_class->finalize = gst_rsfile_sink_finalize;

  gst_element_class_set_static_metadata (gstelement_class,
      "File Sink",
      "Sink/Rsfile",
      "Write to a file",
      "Luis de Bethencourt <luisbg@osg.samsung.com>");
  gst_element_class_add_static_pad_template (gstelement_class, &sink_template);

  gstbasesink_class->start = GST_DEBUG_FUNCPTR (gst_rsfile_sink_start);
  gstbasesink_class->stop = GST_DEBUG_FUNCPTR (gst_rsfile_sink_stop);
  gstbasesink_class->query = GST_DEBUG_FUNCPTR (gst_rsfile_sink_query);
  gstbasesink_class->render = GST_DEBUG_FUNCPTR (gst_rsfile_sink_render);
  gstbasesink_class->event = GST_DEBUG_FUNCPTR (gst_rsfile_sink_event);
}

static void
gst_rsfile_sink_init (GstRsFileSink * src)
{
  gst_base_sink_set_blocksize (GST_BASE_SINK (src), 4096);
}

static void
gst_rsfile_sink_finalize (GObject * object)
{
  GstRsFileSink *src = GST_RSFILE_SINK (object);

  g_free (src->location);

  G_OBJECT_CLASS (parent_class)->finalize (object);
}

static void
gst_rsfile_sink_set_property (GObject * object, guint prop_id,
    const GValue * value, GParamSpec * pspec)
{
  GstRsFileSink *src = GST_RSFILE_SINK (object);

  switch (prop_id) {
    case PROP_LOCATION:
      if (src->location)
        g_free (src->location);
      src->location = g_value_dup_string (value);
      break;
    default:
      G_OBJECT_WARN_INVALID_PROPERTY_ID (object, prop_id, pspec);
      break;
  }
}

static void
gst_rsfile_sink_get_property (GObject * object, guint prop_id, GValue * value,
    GParamSpec * pspec)
{
  GstRsFileSink *src = GST_RSFILE_SINK (object);

  switch (prop_id) {
    case PROP_LOCATION:
      g_value_set_string (value, src->location);
      break;
    default:
      G_OBJECT_WARN_INVALID_PROPERTY_ID (object, prop_id, pspec);
      break;
  }
}

extern void bar(void);

static GstFlowReturn
gst_rsfile_sink_render (GstBaseSink * sink, GstBuffer * buffer)
{
  bar();
}

static gboolean
gst_rsfile_sink_query (GstBaseSink * basesink, GstQuery * query)
{
  GstRsFileSink *sink = GST_RSFILE_SINK (basesink);

  return TRUE;
}

static gboolean
gst_rsfile_sink_event (GstBaseSink * sink, GstEvent * event)
{
  return TRUE;
}

/* open the rsfile, necessary to go to READY state */
static gboolean
gst_rsfile_sink_start (GstBaseSink * basesink)
{
  GstRsFileSink *src = GST_RSFILE_SINK (basesink);

  return TRUE;
}

/* unmap and close the rsfile */
static gboolean
gst_rsfile_sink_stop (GstBaseSink * basesink)
{
  GstRsFileSink *src = GST_RSFILE_SINK (basesink);

  return TRUE;
}
