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
