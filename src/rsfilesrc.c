#include "rsfilesrc.h"

#include <string.h>
#include <stdint.h>

/* Declarations for Rust code */
extern void * filesrc_new (void);
extern void filesrc_drop (void * filesrc);
extern GstFlowReturn filesrc_fill (void * filesrc, void * data, size_t * data_len);
extern void filesrc_set_location (void * filesrc, const char *location);
extern char * filesrc_get_location (void * filesrc);
extern uint64_t filesrc_get_size (void * filesrc);
extern gboolean filesrc_is_seekable (void * filesrc);
extern gboolean filesrc_start (void * filesrc);
extern gboolean filesrc_stop (void * filesrc);

GST_DEBUG_CATEGORY_STATIC (gst_rsfile_src_debug);
#define GST_CAT_DEFAULT gst_rsfile_src_debug

static GstStaticPadTemplate src_template = GST_STATIC_PAD_TEMPLATE ("src",
    GST_PAD_SRC,
    GST_PAD_ALWAYS,
    GST_STATIC_CAPS_ANY);

enum
{
  PROP_0,
  PROP_LOCATION
};

static void gst_rsfile_src_uri_handler_init (gpointer g_iface,
    gpointer iface_data);

static void gst_rsfile_src_finalize (GObject * object);

static void gst_rsfile_src_set_property (GObject * object, guint prop_id,
    const GValue * value, GParamSpec * pspec);
static void gst_rsfile_src_get_property (GObject * object, guint prop_id,
    GValue * value, GParamSpec * pspec);

static gboolean gst_rsfile_src_start (GstBaseSrc * basesrc);
static gboolean gst_rsfile_src_stop (GstBaseSrc * basesrc);

static gboolean gst_rsfile_src_is_seekable (GstBaseSrc * src);
static gboolean gst_rsfile_src_get_size (GstBaseSrc * src, guint64 * size);
static GstFlowReturn gst_rsfile_src_fill (GstBaseSrc * src, guint64 offset,
    guint length, GstBuffer * buf);

#define _do_init \
  G_IMPLEMENT_INTERFACE (GST_TYPE_URI_HANDLER, gst_rsfile_src_uri_handler_init); \
  GST_DEBUG_CATEGORY_INIT (gst_rsfile_src_debug, "rsfilesrc", 0, "rsfilesrc element");
#define gst_rsfile_src_parent_class parent_class
G_DEFINE_TYPE_WITH_CODE (GstRsfileSrc, gst_rsfile_src, GST_TYPE_BASE_SRC, _do_init);

static void
gst_rsfile_src_class_init (GstRsfileSrcClass * klass)
{
  GObjectClass *gobject_class;
  GstElementClass *gstelement_class;
  GstBaseSrcClass *gstbasesrc_class;

  gobject_class = G_OBJECT_CLASS (klass);
  gstelement_class = GST_ELEMENT_CLASS (klass);
  gstbasesrc_class = GST_BASE_SRC_CLASS (klass);

  gobject_class->set_property = gst_rsfile_src_set_property;
  gobject_class->get_property = gst_rsfile_src_get_property;

  g_object_class_install_property (gobject_class, PROP_LOCATION,
      g_param_spec_string ("location", "File Location",
          "Location of the file to read", NULL,
          G_PARAM_READWRITE | G_PARAM_STATIC_STRINGS |
          GST_PARAM_MUTABLE_READY));

  gobject_class->finalize = gst_rsfile_src_finalize;

  gst_element_class_set_static_metadata (gstelement_class,
      "File Source",
      "Source/Rsfile",
      "Read from arbitrary point in a file",
      "Sebastian Dröge <sebastian@centricular.com>");
  gst_element_class_add_static_pad_template (gstelement_class, &src_template);

  gstbasesrc_class->start = GST_DEBUG_FUNCPTR (gst_rsfile_src_start);
  gstbasesrc_class->stop = GST_DEBUG_FUNCPTR (gst_rsfile_src_stop);
  gstbasesrc_class->is_seekable = GST_DEBUG_FUNCPTR (gst_rsfile_src_is_seekable);
  gstbasesrc_class->get_size = GST_DEBUG_FUNCPTR (gst_rsfile_src_get_size);
  gstbasesrc_class->fill = GST_DEBUG_FUNCPTR (gst_rsfile_src_fill);
}

static void
gst_rsfile_src_init (GstRsfileSrc * src)
{
  gst_base_src_set_blocksize (GST_BASE_SRC (src), 4096);

  src->instance = filesrc_new ();
}

static void
gst_rsfile_src_finalize (GObject * object)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (object);

  filesrc_drop (src->instance);

  G_OBJECT_CLASS (parent_class)->finalize (object);
}

static void
gst_rsfile_src_set_property (GObject * object, guint prop_id,
    const GValue * value, GParamSpec * pspec)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (object);

  switch (prop_id) {
    case PROP_LOCATION:
      filesrc_set_location (src->instance, g_value_get_string (value));
      break;
    default:
      G_OBJECT_WARN_INVALID_PROPERTY_ID (object, prop_id, pspec);
      break;
  }
}

static void
gst_rsfile_src_get_property (GObject * object, guint prop_id, GValue * value,
    GParamSpec * pspec)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (object);

  switch (prop_id) {
    case PROP_LOCATION:
      g_value_take_string (value, filesrc_get_location (src->instance));
      break;
    default:
      G_OBJECT_WARN_INVALID_PROPERTY_ID (object, prop_id, pspec);
      break;
  }
}

static GstFlowReturn
gst_rsfile_src_fill (GstBaseSrc * basesrc, guint64 offset, guint length,
    GstBuffer * buf)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (basesrc);
  GstMapInfo map;
  GstFlowReturn ret;
  gsize size;

  gst_buffer_map (buf, &map, GST_MAP_READWRITE);
  size = map.size;
  ret = filesrc_fill (src->instance, map.data, &size);
  gst_buffer_unmap (buf, &map);
  if (ret == GST_FLOW_OK)
    gst_buffer_resize (buf, 0, size);

  return ret;
}

static gboolean
gst_rsfile_src_is_seekable (GstBaseSrc * basesrc)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (basesrc);

  return filesrc_is_seekable (src->instance);
}

static gboolean
gst_rsfile_src_get_size (GstBaseSrc * basesrc, guint64 * size)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (basesrc);

  *size = filesrc_get_size (src->instance);

  return TRUE;
}

/* open the rsfile, necessary to go to READY state */
static gboolean
gst_rsfile_src_start (GstBaseSrc * basesrc)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (basesrc);

  return filesrc_start (src->instance);
}

/* unmap and close the rsfile */
static gboolean
gst_rsfile_src_stop (GstBaseSrc * basesrc)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (basesrc);

  return filesrc_stop (src->instance);
}

static GstURIType
gst_rsfile_src_uri_get_type (GType type)
{
  return GST_URI_SRC;
}

static const gchar *const *
gst_rsfile_src_uri_get_protocols (GType type)
{
  static const gchar *protocols[] = { "file", NULL };

  return protocols;
}

static gchar *
gst_rsfile_src_uri_get_uri (GstURIHandler * handler)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (handler);
  gchar *location, *uri;

  location = filesrc_get_location (src->instance);
  if (!location)
    return NULL;
  uri = gst_filename_to_uri (location, NULL);
  g_free (location);

  return uri;
}

static gboolean
gst_rsfile_src_uri_set_uri (GstURIHandler * handler, const gchar * uri,
    GError ** err)
{
  gchar *location, *hostname = NULL;
  gboolean ret = FALSE;
  GstRsfileSrc *src = GST_RSFILE_SRC (handler);

  if (strcmp (uri, "file://") == 0) {
    /* Special case for "file://" as this is used by some applications
     *  to test with gst_element_make_from_uri if there's an element
     *  that supports the URI protocol. */
    filesrc_set_location (src->instance, location);
    return TRUE;
  }

  location = g_filename_from_uri (uri, &hostname, err);

  if (!location || (err != NULL && *err != NULL)) {
    GST_WARNING_OBJECT (src, "Invalid URI '%s' for filesrc: %s", uri,
        (err != NULL && *err != NULL) ? (*err)->message : "unknown error");
    goto beach;
  }

  if ((hostname) && (strcmp (hostname, "localhost"))) {
    /* Only 'localhost' is permitted */
    GST_WARNING_OBJECT (src, "Invalid hostname '%s' for filesrc", hostname);
    g_set_error (err, GST_URI_ERROR, GST_URI_ERROR_BAD_URI,
        "File URI with invalid hostname '%s'", hostname);
    goto beach;
  }
#ifdef G_OS_WIN32
  /* Unfortunately, g_filename_from_uri() doesn't handle some UNC paths
   * correctly on windows, it leaves them with an extra backslash
   * at the start if they're of the mozilla-style file://///host/path/file 
   * form. Correct this.
   */
  if (location[0] == '\\' && location[1] == '\\' && location[2] == '\\')
    memmove (location, location + 1, strlen (location + 1) + 1);
#endif

  ret = TRUE;
  filesrc_set_location (src->instance, location);

beach:
  if (location)
    g_free (location);
  if (hostname)
    g_free (hostname);

  return ret;
}

static void
gst_rsfile_src_uri_handler_init (gpointer g_iface, gpointer iface_data)
{
  GstURIHandlerInterface *iface = (GstURIHandlerInterface *) g_iface;

  iface->get_type = gst_rsfile_src_uri_get_type;
  iface->get_protocols = gst_rsfile_src_uri_get_protocols;
  iface->get_uri = gst_rsfile_src_uri_get_uri;
  iface->set_uri = gst_rsfile_src_uri_set_uri;
}
