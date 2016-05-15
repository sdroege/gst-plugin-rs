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
