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
gst_rs_element_error (GstElement * element, GQuark error_domain,
    gint error_code, const gchar * message, const gchar * debug,
    const gchar * file, const gchar * function, guint line)
{
  gst_element_message_full (element, GST_MESSAGE_ERROR, error_domain,
      error_code, g_strdup (message), g_strdup (debug), file, function, line);
}
