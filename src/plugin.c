#include <gst/gst.h>

#include "rsfilesrc.h"
#include "rsfilesink.h"

static gboolean
plugin_init (GstPlugin * plugin)
{
  if (!gst_element_register (plugin, "rsfilesrc", GST_RANK_PRIMARY+100,
          GST_TYPE_RSFILE_SRC)) {
    return FALSE;
  }

  if (!gst_element_register (plugin, "rsfilesink", GST_RANK_NONE,
          GST_TYPE_RSFILE_SINK)) {
    return FALSE;
  }

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
