#include "rsfilesrc.h"

/* Declarations for Rust code */
extern void * filesrc_new (void);
extern void filesrc_drop (void * filesrc);
extern int filesrc_fill (void * filesrc);

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
  g_free (src->location);

  G_OBJECT_CLASS (parent_class)->finalize (object);
}

static void
gst_rsfile_src_set_property (GObject * object, guint prop_id,
    const GValue * value, GParamSpec * pspec)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (object);

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
gst_rsfile_src_get_property (GObject * object, guint prop_id, GValue * value,
    GParamSpec * pspec)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (object);

  switch (prop_id) {
    case PROP_LOCATION:
      g_value_set_string (value, src->location);
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

  g_print ("foo %p\n", src->instance);
  filesrc_fill (src->instance);

  gst_buffer_memset (buf, 0, 0, gst_buffer_get_size (buf));

  return GST_FLOW_OK;
}

static gboolean
gst_rsfile_src_is_seekable (GstBaseSrc * basesrc)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (basesrc);

  return TRUE;
}

static gboolean
gst_rsfile_src_get_size (GstBaseSrc * basesrc, guint64 * size)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (basesrc);

  *size = -1;

  return TRUE;
}

/* open the rsfile, necessary to go to READY state */
static gboolean
gst_rsfile_src_start (GstBaseSrc * basesrc)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (basesrc);

  return TRUE;
}

/* unmap and close the rsfile */
static gboolean
gst_rsfile_src_stop (GstBaseSrc * basesrc)
{
  GstRsfileSrc *src = GST_RSFILE_SRC (basesrc);

  return TRUE;
}


