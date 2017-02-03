/* Copyright (C) 2016 Sebastian Dröge <sebastian@centricular.com>
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
