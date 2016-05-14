#include "rssink.h"

#include <string.h>
#include <stdint.h>

typedef struct {
  gchar *name;
  gchar *long_name;
  gchar *description;
  gchar *classification;
  gchar *author;
  void * (*create_instance) (void);
  gchar **protocols;
} ElementData;
static GHashTable *sinks;

/* Declarations for Rust code */
extern gboolean sinks_register (void *plugin);
extern GstFlowReturn sink_render (void * filesink);
extern gboolean sink_set_uri (void * filesink, const char *uri);
extern char * sink_get_uri (void * filesink);
extern gboolean sink_start (void * filesink);
extern gboolean sink_stop (void * filesink);

GST_DEBUG_CATEGORY_STATIC (gst_rs_sink_debug);
#define GST_CAT_DEFAULT gst_rs_sink_debug

static GstStaticPadTemplate sink_template = GST_STATIC_PAD_TEMPLATE ("sink",
    GST_PAD_SINK,
    GST_PAD_ALWAYS,
    GST_STATIC_CAPS_ANY);

enum
{
  PROP_0,
  PROP_URI
};

static void gst_rs_sink_uri_handler_init (gpointer g_iface,
    gpointer iface_data);

static void gst_rs_sink_set_property (GObject * object, guint prop_id,
    const GValue * value, GParamSpec * pspec);
static void gst_rs_sink_get_property (GObject * object, guint prop_id,
    GValue * value, GParamSpec * pspec);

static gboolean gst_rs_sink_start (GstBaseSink * basesink);
static gboolean gst_rs_sink_stop (GstBaseSink * basesink);

static GstFlowReturn gst_rs_sink_render (GstBaseSink * sink, GstBuffer * buffer);

static GObjectClass *parent_class;

static void
gst_rs_sink_class_init (GstRsSinkClass * klass)
{
  GObjectClass *gobject_class;
  GstElementClass *gstelement_class;
  GstBaseSinkClass *gstbasesink_class;
  ElementData * data = g_hash_table_lookup (sinks, GSIZE_TO_POINTER (G_TYPE_FROM_CLASS (klass)));
  g_assert (data != NULL);

  gobject_class = G_OBJECT_CLASS (klass);
  gstelement_class = GST_ELEMENT_CLASS (klass);
  gstbasesink_class = GST_BASE_SINK_CLASS (klass);

  gobject_class->set_property = gst_rs_sink_set_property;
  gobject_class->get_property = gst_rs_sink_get_property;

  g_object_class_install_property (gobject_class, PROP_URI,
      g_param_spec_string ("uri", "URI",
          "URI to read from", NULL,
          G_PARAM_READWRITE | G_PARAM_STATIC_STRINGS |
          GST_PARAM_MUTABLE_READY));

  gst_element_class_set_static_metadata (gstelement_class,
      data->long_name,
      data->classification,
      data->description,
      data->author);
  gst_element_class_add_static_pad_template (gstelement_class, &sink_template);

  gstbasesink_class->start = GST_DEBUG_FUNCPTR (gst_rs_sink_start);
  gstbasesink_class->stop = GST_DEBUG_FUNCPTR (gst_rs_sink_stop);
  gstbasesink_class->render = GST_DEBUG_FUNCPTR (gst_rs_sink_render);
}

static void
gst_rs_sink_init (GstRsSink * sink, GstRsSinkClass * klass)
{
  ElementData * data = g_hash_table_lookup (sinks, GSIZE_TO_POINTER (G_TYPE_FROM_CLASS (klass)));
  g_assert (data != NULL);

  gst_base_sink_set_sync (GST_BASE_SINK (sink), FALSE);

  sink->instance = data->create_instance ();
}

static void
gst_rs_sink_set_property (GObject * object, guint prop_id,
    const GValue * value, GParamSpec * pspec)
{
  GstRsSink *sink = GST_RS_SINK (object);

  switch (prop_id) {
    case PROP_URI:
      sink_set_uri (sink->instance, g_value_get_string (value));
      break;
    default:
      G_OBJECT_WARN_INVALID_PROPERTY_ID (object, prop_id, pspec);
      break;
  }
}

static void
gst_rs_sink_get_property (GObject * object, guint prop_id, GValue * value,
    GParamSpec * pspec)
{
  GstRsSink *sink = GST_RS_SINK (object);

  switch (prop_id) {
    case PROP_URI:
      g_value_take_string (value, sink_get_uri (sink->instance));
      break;
    default:
      G_OBJECT_WARN_INVALID_PROPERTY_ID (object, prop_id, pspec);
      break;
  }
}

static GstFlowReturn
gst_rs_sink_render (GstBaseSink * basesink, GstBuffer * buffer)
{
  GstRsSink *sink = GST_RS_SINK (basesink);
  GstFlowReturn ret;

  ret = sink_render (sink->instance);

  return ret;
}

/* open the rs, necessary to go to READY state */
static gboolean
gst_rs_sink_start (GstBaseSink * basesink)
{
  GstRsSink *sink = GST_RS_SINK (basesink);

  return sink_start (sink->instance);
}

/* unmap and close the rs */
static gboolean
gst_rs_sink_stop (GstBaseSink * basesink)
{
  GstRsSink *sink = GST_RS_SINK (basesink);

  return sink_stop (sink->instance);
}

static GstURIType
gst_rs_sink_uri_get_type (GType type)
{
  return GST_URI_SINK;
}

static const gchar *const *
gst_rs_sink_uri_get_protocols (GType type)
{
  ElementData * data = g_hash_table_lookup (sinks, GSIZE_TO_POINTER (type));
  g_assert (data != NULL);

  return (const gchar * const *) data->protocols;
}

static gchar *
gst_rs_sink_uri_get_uri (GstURIHandler * handler)
{
  GstRsSink *sink = GST_RS_SINK (handler);

  return sink_get_uri (sink->instance);
}

static gboolean
gst_rs_sink_uri_set_uri (GstURIHandler * handler, const gchar * uri,
    GError ** err)
{
  GstRsSink *sink = GST_RS_SINK (handler);

  if (!sink_set_uri (sink->instance, uri)) {
    g_set_error (err, GST_URI_ERROR, GST_URI_ERROR_BAD_URI,
          "Can't handle URI '%s'", uri);
    return FALSE;
  }

  return TRUE;
}

static void
gst_rs_sink_uri_handler_init (gpointer g_iface, gpointer iface_data)
{
  GstURIHandlerInterface *iface = (GstURIHandlerInterface *) g_iface;

  iface->get_type = gst_rs_sink_uri_get_type;
  iface->get_protocols = gst_rs_sink_uri_get_protocols;
  iface->get_uri = gst_rs_sink_uri_get_uri;
  iface->set_uri = gst_rs_sink_uri_set_uri;
}

gboolean
gst_rs_sink_plugin_init (GstPlugin * plugin)
{
  sinks = g_hash_table_new (g_direct_hash, g_direct_equal);
  GST_DEBUG_CATEGORY_INIT (gst_rs_sink_debug, "rssink", 0, "rssink element");

  parent_class = g_type_class_ref (GST_TYPE_BASE_SINK);

  return sinks_register (plugin);
}

gboolean
gst_rs_sink_register (GstPlugin * plugin, const gchar *name, const gchar * long_name, const gchar * description, const gchar * classification, const gchar * author, GstRank rank, void * (*create_instance) (void), const gchar *protocols)
{
  GTypeInfo type_info = {
    sizeof (GstRsSinkClass),
    NULL,
    NULL,
    (GClassInitFunc) gst_rs_sink_class_init,
    NULL,
    NULL,
    sizeof (GstRsSink),
    0,
    (GInstanceInitFunc) gst_rs_sink_init
  };
  GInterfaceInfo iface_info = {
    gst_rs_sink_uri_handler_init,
    NULL,
    NULL
  };
  GType type;
  gchar *type_name;
  ElementData *data;

  data = g_new0 (ElementData, 1);
  data->name = g_strdup (name);
  data->long_name = g_strdup (long_name);
  data->description = g_strdup (description);
  data->classification = g_strdup (classification);
  data->author = g_strdup (author);
  data->create_instance = create_instance;
  data->protocols = g_strsplit (protocols, ":", -1);

  type_name = g_strconcat ("RsSink-", name, NULL);
  type = g_type_register_static (GST_TYPE_BASE_SINK, type_name, &type_info, 0);
  g_free (type_name);

  g_type_add_interface_static (type, GST_TYPE_URI_HANDLER, &iface_info);

  g_hash_table_insert (sinks, GSIZE_TO_POINTER (type), data);

  if (!gst_element_register (plugin, name, rank, type))
    return FALSE;

  return TRUE;
}
