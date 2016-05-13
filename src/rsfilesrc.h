#ifndef __GST_RSFILE_SRC_H__
#define __GST_RSFILE_SRC_H__

#include <gst/gst.h>
#include <gst/base/gstbasesrc.h>

G_BEGIN_DECLS

#define GST_TYPE_RSFILE_SRC \
  (gst_rsfile_src_get_type())
#define GST_RSFILE_SRC(obj) \
  (G_TYPE_CHECK_INSTANCE_CAST((obj),GST_TYPE_RSFILE_SRC,GstRsfileSrc))
#define GST_RSFILE_SRC_CLASS(klass) \
  (G_TYPE_CHECK_CLASS_CAST((klass),GST_TYPE_RSFILE_SRC,GstRsfileSrcClass))
#define GST_IS_RSFILE_SRC(obj) \
  (G_TYPE_CHECK_INSTANCE_TYPE((obj),GST_TYPE_RSFILE_SRC))
#define GST_IS_RSFILE_SRC_CLASS(klass) \
  (G_TYPE_CHECK_CLASS_TYPE((klass),GST_TYPE_RSFILE_SRC))
#define GST_RSFILE_SRC_CAST(obj) ((GstRsfileSrc*) obj)

typedef struct _GstRsfileSrc GstRsfileSrc;
typedef struct _GstRsfileSrcClass GstRsfileSrcClass;

struct _GstRsfileSrc {
  GstBaseSrc element;

  gchar *location;
};

struct _GstRsfileSrcClass {
  GstBaseSrcClass parent_class;
};

G_GNUC_INTERNAL GType gst_rsfile_src_get_type (void);

G_END_DECLS

#endif /* __GST_RSFILE_SRC_H__ */
