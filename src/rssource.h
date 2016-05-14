#ifndef __GST_RS_SRC_H__
#define __GST_RS_SRC_H__

#include <gst/gst.h>
#include <gst/base/base.h>

G_BEGIN_DECLS

#define GST_RS_SRC(obj) \
  ((GstRsSrc *)obj)
#define GST_RS_SRC_CLASS(klass) \
  ((GstRsSrcKlass *)klass)

typedef struct _GstRsSrc GstRsSrc;
typedef struct _GstRsSrcClass GstRsSrcClass;

struct _GstRsSrc {
  GstPushSrc element;

  gpointer instance;
};

struct _GstRsSrcClass {
  GstPushSrcClass parent_class;
};

G_GNUC_INTERNAL gboolean gst_rs_source_plugin_init (GstPlugin * plugin);

G_END_DECLS

#endif /* __GST_RS_SRC_H__ */
