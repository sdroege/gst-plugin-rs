/* Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
 *
 * Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
 * http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
 * <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
 * option. This file may not be copied, modified, or distributed
 * except according to those terms.
 */

#include <gst/gst.h>

void
gst_rs_debug_log (GstDebugCategory * category,
    GstDebugLevel level,
    const gchar * file,
    const gchar * function, gint line, GObject * object, const gchar * message)
{
  gst_debug_log (category, level, file, function, line, object, "%s", message);
}
