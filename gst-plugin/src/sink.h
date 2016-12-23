/* Copyright (C) 2016 Sebastian Dröge <sebastian@centricular.com>
 *               2016 Luis de Bethencourt <luisbg@osg.samsung.com>
 *
 * This library is free software; you can redistribute it and/or
 * modify it under the terms of the GNU Library General Public
 * License as published by the Free Software Foundation; either
 * version 2 of the License, or (at your option) any later version.
 *
 * This library is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
 * Library General Public License for more details.
 *
 * You should have received a copy of the GNU Library General Public
 * License along with this library; if not, write to the
 * Free Software Foundation, Inc., 51 Franklin St, Fifth Floor,
 * Boston, MA 02110-1301, USA.
 */

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
