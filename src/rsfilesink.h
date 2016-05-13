#ifndef __GST_RSFILE_SINK_H__
#define __GST_RSFILE_SINK_H__

#include <gst/gst.h>
#include <gst/base/gstbasesink.h>

G_BEGIN_DECLS

#define GST_TYPE_RSFILE_SINK \
  (gst_rsfile_sink_get_type())
#define GST_RSFILE_SINK(obj) \
  (G_TYPE_CHECK_INSTANCE_CAST((obj),GST_TYPE_RSFILE_SINK,GstRsFileSink))
#define GST_RSFILE_SINK_CLASS(klass) \
  (G_TYPE_CHECK_CLASS_CAST((klass),GST_TYPE_RSFILE_SINK,GstRsFileSinkClass))
#define GST_IS_RSFILE_SINK(obj) \
  (G_TYPE_CHECK_INSTANCE_TYPE((obj),GST_TYPE_RSFILE_SINK))
#define GST_IS_RSFILE_SINK_CLASS(klass) \
  (G_TYPE_CHECK_CLASS_TYPE((klass),GST_TYPE_RSFILE_SINK))
#define GST_RSFILE_SINK_CAST(obj) ((GstRsFileSink*) obj)

typedef struct _GstRsFileSink GstRsFileSink;
typedef struct _GstRsFileSinkClass GstRsFileSinkClass;

struct _GstRsFileSink {
  GstBaseSink element;

  gchar *location;
};

struct _GstRsFileSinkClass {
  GstBaseSinkClass parent_class;
};

G_GNUC_INTERNAL GType gst_rsfile_sink_get_type (void);

G_END_DECLS

#endif /* __GST_RSFILE_SINK_H__ */
