/* Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
 *
 * Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
 * http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
 * <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
 * option. This file may not be copied, modified, or distributed
 * except according to those terms.
 */

#include <gst/gst.h>

guint64
gst_rs_buffer_get_pts (GstBuffer * buffer)
{
  return GST_BUFFER_PTS (buffer);
}

void
gst_rs_buffer_set_pts (GstBuffer * buffer, guint64 pts)
{
  GST_BUFFER_PTS (buffer) = pts;
}

guint64
gst_rs_buffer_get_dts (GstBuffer * buffer)
{
  return GST_BUFFER_DTS (buffer);
}

void
gst_rs_buffer_set_dts (GstBuffer * buffer, guint64 dts)
{
  GST_BUFFER_DTS (buffer) = dts;
}

guint64
gst_rs_buffer_get_duration (GstBuffer * buffer)
{
  return GST_BUFFER_DURATION (buffer);
}

void
gst_rs_buffer_set_duration (GstBuffer * buffer, guint64 dts)
{
  GST_BUFFER_DURATION (buffer) = dts;
}

guint64
gst_rs_buffer_get_offset (GstBuffer * buffer)
{
  return GST_BUFFER_OFFSET (buffer);
}

void
gst_rs_buffer_set_offset (GstBuffer * buffer, guint64 offset)
{
  GST_BUFFER_OFFSET (buffer) = offset;
}

guint64
gst_rs_buffer_get_offset_end (GstBuffer * buffer)
{
  return GST_BUFFER_OFFSET_END (buffer);
}

void
gst_rs_buffer_set_offset_end (GstBuffer * buffer, guint64 offset_end)
{
  GST_BUFFER_OFFSET_END (buffer) = offset_end;
}

GstBufferFlags
gst_rs_buffer_get_flags (GstBuffer * buffer)
{
  return GST_MINI_OBJECT_FLAGS (buffer);
}

void
gst_rs_buffer_set_flags (GstBuffer * buffer, GstBufferFlags flags)
{
  GST_MINI_OBJECT_FLAGS (buffer) = flags;
}
