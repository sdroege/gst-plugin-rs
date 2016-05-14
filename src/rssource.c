#include "rssource.h"

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
static GHashTable *sources;

/* Declarations for Rust code */
extern gboolean sources_register (void *plugin);
extern void source_drop (void * filesrc);
extern GstFlowReturn source_fill (void * filesrc, uint64_t offset, void * data, size_t * data_len);
extern gboolean source_set_uri (void * filesrc, const char *uri);
extern char * source_get_uri (void * filesrc);
extern uint64_t source_get_size (void * filesrc);
extern gboolean source_is_seekable (void * filesrc);
extern gboolean source_start (void * filesrc);
extern gboolean source_stop (void * filesrc);

GST_DEBUG_CATEGORY_STATIC (gst_rs_src_debug);
#define GST_CAT_DEFAULT gst_rs_src_debug

static GstStaticPadTemplate src_template = GST_STATIC_PAD_TEMPLATE ("src",
    GST_PAD_SRC,
    GST_PAD_ALWAYS,
    GST_STATIC_CAPS_ANY);

enum
{
  PROP_0,
  PROP_URI
};

static void gst_rs_src_uri_handler_init (gpointer g_iface,
    gpointer iface_data);

static void gst_rs_src_finalize (GObject * object);

static void gst_rs_src_set_property (GObject * object, guint prop_id,
    const GValue * value, GParamSpec * pspec);
static void gst_rs_src_get_property (GObject * object, guint prop_id,
    GValue * value, GParamSpec * pspec);

static gboolean gst_rs_src_start (GstBaseSrc * basesrc);
static gboolean gst_rs_src_stop (GstBaseSrc * basesrc);

static gboolean gst_rs_src_is_seekable (GstBaseSrc * src);
static gboolean gst_rs_src_get_size (GstBaseSrc * src, guint64 * size);
static GstFlowReturn gst_rs_src_fill (GstBaseSrc * src, guint64 offset,
    guint length, GstBuffer * buf);

static GObjectClass *parent_class;

static void
gst_rs_src_class_init (GstRsSrcClass * klass)
{
  GObjectClass *gobject_class;
  GstElementClass *gstelement_class;
  GstBaseSrcClass *gstbasesrc_class;
  ElementData * data = g_hash_table_lookup (sources, GSIZE_TO_POINTER (G_TYPE_FROM_CLASS (klass)));
  g_assert (data != NULL);

  gobject_class = G_OBJECT_CLASS (klass);
  gstelement_class = GST_ELEMENT_CLASS (klass);
  gstbasesrc_class = GST_BASE_SRC_CLASS (klass);

  gobject_class->set_property = gst_rs_src_set_property;
  gobject_class->get_property = gst_rs_src_get_property;

  g_object_class_install_property (gobject_class, PROP_URI,
      g_param_spec_string ("uri", "URI",
          "URI to read from", NULL,
          G_PARAM_READWRITE | G_PARAM_STATIC_STRINGS |
          GST_PARAM_MUTABLE_READY));

  gobject_class->finalize = gst_rs_src_finalize;

  gst_element_class_set_static_metadata (gstelement_class,
      data->long_name,
      data->classification,
      data->description,
      data->author);
  gst_element_class_add_static_pad_template (gstelement_class, &src_template);

  gstbasesrc_class->start = GST_DEBUG_FUNCPTR (gst_rs_src_start);
  gstbasesrc_class->stop = GST_DEBUG_FUNCPTR (gst_rs_src_stop);
  gstbasesrc_class->is_seekable = GST_DEBUG_FUNCPTR (gst_rs_src_is_seekable);
  gstbasesrc_class->get_size = GST_DEBUG_FUNCPTR (gst_rs_src_get_size);
  gstbasesrc_class->fill = GST_DEBUG_FUNCPTR (gst_rs_src_fill);
}

static void
gst_rs_src_init (GstRsSrc * src, GstRsSrcClass * klass)
{
  ElementData * data = g_hash_table_lookup (sources, GSIZE_TO_POINTER (G_TYPE_FROM_CLASS (klass)));
  g_assert (data != NULL);

  gst_base_src_set_blocksize (GST_BASE_SRC (src), 4096);

  src->instance = data->create_instance ();
}

static void
gst_rs_src_finalize (GObject * object)
{
  GstRsSrc *src = GST_RS_SRC (object);

  source_drop (src->instance);

  G_OBJECT_CLASS (parent_class)->finalize (object);
}

static void
gst_rs_src_set_property (GObject * object, guint prop_id,
    const GValue * value, GParamSpec * pspec)
{
  GstRsSrc *src = GST_RS_SRC (object);

  switch (prop_id) {
    case PROP_URI:
      source_set_uri (src->instance, g_value_get_string (value));
      break;
    default:
      G_OBJECT_WARN_INVALID_PROPERTY_ID (object, prop_id, pspec);
      break;
  }
}

static void
gst_rs_src_get_property (GObject * object, guint prop_id, GValue * value,
    GParamSpec * pspec)
{
  GstRsSrc *src = GST_RS_SRC (object);

  switch (prop_id) {
    case PROP_URI:
      g_value_take_string (value, source_get_uri (src->instance));
      break;
    default:
      G_OBJECT_WARN_INVALID_PROPERTY_ID (object, prop_id, pspec);
      break;
  }
}

static GstFlowReturn
gst_rs_src_fill (GstBaseSrc * basesrc, guint64 offset, guint length,
    GstBuffer * buf)
{
  GstRsSrc *src = GST_RS_SRC (basesrc);
  GstMapInfo map;
  GstFlowReturn ret;
  gsize map_size, size;

  gst_buffer_map (buf, &map, GST_MAP_READWRITE);
  size = length;
  map_size = map.size;
  ret = source_fill (src->instance, offset, map.data, &size);
  gst_buffer_unmap (buf, &map);
  if (ret == GST_FLOW_OK && size != map_size)
    gst_buffer_resize (buf, 0, size);

  return ret;
}

static gboolean
gst_rs_src_is_seekable (GstBaseSrc * basesrc)
{
  GstRsSrc *src = GST_RS_SRC (basesrc);

  return source_is_seekable (src->instance);
}

static gboolean
gst_rs_src_get_size (GstBaseSrc * basesrc, guint64 * size)
{
  GstRsSrc *src = GST_RS_SRC (basesrc);

  *size = source_get_size (src->instance);

  return TRUE;
}

/* open the rs, necessary to go to READY state */
static gboolean
gst_rs_src_start (GstBaseSrc * basesrc)
{
  GstRsSrc *src = GST_RS_SRC (basesrc);

  return source_start (src->instance);
}

/* unmap and close the rs */
static gboolean
gst_rs_src_stop (GstBaseSrc * basesrc)
{
  GstRsSrc *src = GST_RS_SRC (basesrc);

  return source_stop (src->instance);
}

static GstURIType
gst_rs_src_uri_get_type (GType type)
{
  return GST_URI_SRC;
}

static const gchar *const *
gst_rs_src_uri_get_protocols (GType type)
{
  ElementData * data = g_hash_table_lookup (sources, GSIZE_TO_POINTER (type));
  g_assert (data != NULL);

  return (const gchar * const *) data->protocols;
}

static gchar *
gst_rs_src_uri_get_uri (GstURIHandler * handler)
{
  GstRsSrc *src = GST_RS_SRC (handler);

  return source_get_uri (src->instance);
}

static gboolean
gst_rs_src_uri_set_uri (GstURIHandler * handler, const gchar * uri,
    GError ** err)
{
  GstRsSrc *src = GST_RS_SRC (handler);

  if (!source_set_uri (src->instance, uri)) {
    g_set_error (err, GST_URI_ERROR, GST_URI_ERROR_BAD_URI,
          "Can't handle URI '%s'", uri);
    return FALSE;
  }

  return TRUE;
}

static void
gst_rs_src_uri_handler_init (gpointer g_iface, gpointer iface_data)
{
  GstURIHandlerInterface *iface = (GstURIHandlerInterface *) g_iface;

  iface->get_type = gst_rs_src_uri_get_type;
  iface->get_protocols = gst_rs_src_uri_get_protocols;
  iface->get_uri = gst_rs_src_uri_get_uri;
  iface->set_uri = gst_rs_src_uri_set_uri;
}

gboolean
gst_rs_source_plugin_init (GstPlugin * plugin)
{
  sources = g_hash_table_new (g_direct_hash, g_direct_equal);
  GST_DEBUG_CATEGORY_INIT (gst_rs_src_debug, "rssrc", 0, "rssrc element");

  parent_class = g_type_class_ref (GST_TYPE_BASE_SRC);

  return sources_register (plugin);
}

gboolean
gst_rs_source_register (GstPlugin * plugin, const gchar *name, const gchar * long_name, const gchar * description, const gchar * classification, const gchar * author, GstRank rank, void * (*create_instance) (void), const gchar *protocols, gboolean push_only)
{
  GTypeInfo type_info = {
    sizeof (GstRsSrcClass),
    NULL,
    NULL,
    (GClassInitFunc) gst_rs_src_class_init,
    NULL,
    NULL,
    sizeof (GstRsSrc),
    0,
    (GInstanceInitFunc) gst_rs_src_init
  };
  GInterfaceInfo iface_info = {
    gst_rs_src_uri_handler_init,
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

  type_name = g_strconcat ("RsSrc-", name, NULL);
  type = g_type_register_static (push_only ? GST_TYPE_PUSH_SRC : GST_TYPE_BASE_SRC, type_name, &type_info, 0);
  g_free (type_name);

  g_type_add_interface_static (type, GST_TYPE_URI_HANDLER, &iface_info);

  g_hash_table_insert (sources, GSIZE_TO_POINTER (type), data);

  if (!gst_element_register (plugin, name, rank, type))
    return FALSE;

  return TRUE;
}
