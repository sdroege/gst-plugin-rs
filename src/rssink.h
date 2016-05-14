#ifndef __GST_RS_SINK_H__
#define __GST_RS_SINK_H__

#include <gst/gst.h>
#include <gst/base/gstbasesink.h>

G_BEGIN_DECLS

#define GST_RS_SINK(obj) \
  ((GstRsSink *)obj)
#define GST_RS_SINK_CLASS(klass) \
  ((GstRsSinkKlass *)klass)

typedef struct _GstRsSink GstRsSink;
typedef struct _GstRsSinkClass GstRsSinkClass;

struct _GstRsSink {
  GstBaseSink element;

  gpointer instance;
};

struct _GstRsSinkClass {
  GstBaseSinkClass parent_class;
};

G_GNUC_INTERNAL gboolean gst_rs_sink_plugin_init (GstPlugin * plugin);

G_END_DECLS

#endif /* __GST_RS_SINK_H__ */
