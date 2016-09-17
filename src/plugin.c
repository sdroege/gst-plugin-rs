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

#include "rssource.h"
#include "rssink.h"

static gboolean
plugin_init (GstPlugin * plugin)
{
  if (!gst_rs_source_plugin_init (plugin))
    return FALSE;

  if (!gst_rs_sink_plugin_init (plugin))
    return FALSE;

  return TRUE;
}

#define VERSION "1.0"
#define PACKAGE "rsplugin"
#define GST_PACKAGE_NAME PACKAGE
#define GST_PACKAGE_ORIGIN "http://www.example.org"

GST_PLUGIN_DEFINE (GST_VERSION_MAJOR,
    GST_VERSION_MINOR,
    rsplugin,
    "Rust Plugin", plugin_init, VERSION, "LGPL", GST_PACKAGE_NAME,
    GST_PACKAGE_ORIGIN);

void
gst_rs_element_error (GstElement * element, GQuark error_domain,
    gint error_code, const gchar * message, const gchar * debug,
    const gchar * file, const gchar * function, guint line)
{
  gst_element_message_full (element, GST_MESSAGE_ERROR, error_domain,
      error_code, g_strdup (message), g_strdup (debug), file, function, line);
}

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
gst_rs_buffer_get_buffer_flags (GstBuffer * buffer)
{
  return GST_MINI_OBJECT_FLAGS (buffer);
}

void
gst_rs_buffer_set_buffer_flags (GstBuffer * buffer, GstBufferFlags flags)
{
  GST_MINI_OBJECT_FLAGS (buffer) = flags;
}
