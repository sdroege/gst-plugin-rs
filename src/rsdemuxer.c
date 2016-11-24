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

#include "rsdemuxer.h"

#include <string.h>
#include <stdint.h>

typedef struct
{
  gchar *long_name;
  gchar *description;
  gchar *classification;
  gchar *author;
  void *create_instance;
  gchar *input_format;
  gchar *output_formats;
} ElementData;
static GHashTable *demuxers;

/* Declarations for Rust code */
extern gboolean demuxers_register (void *plugin);
extern void *demuxer_new (GstRsDemuxer * demuxer, void *create_instance);
extern void demuxer_drop (void *rsdemuxer);

extern gboolean demuxer_start (void *rsdemuxer, uint64_t upstream_size,
    gboolean random_access);
extern gboolean demuxer_stop (void *rsdemuxer);

extern gboolean demuxer_is_seekable (void *rsdemuxer);
extern gboolean demuxer_get_position (void *rsdemuxer, uint64_t * position);
extern gboolean demuxer_get_duration (void *rsdemuxer, uint64_t * duration);

extern gboolean demuxer_seek (void *rsdemuxer, uint64_t start, uint64_t stop,
    uint64_t * offset);
extern GstFlowReturn demuxer_handle_buffer (void *rsdemuxer,
    GstBuffer * buffer);
extern void demuxer_end_of_stream (void *rsdemuxer);

extern void cstring_drop (void *str);

GST_DEBUG_CATEGORY_STATIC (gst_rs_demuxer_debug);
#define GST_CAT_DEFAULT gst_rs_demuxer_debug

static void gst_rs_demuxer_finalize (GObject * object);
static gboolean gst_rs_demuxer_sink_activate (GstPad * pad, GstObject * parent);
static gboolean gst_rs_demuxer_sink_activate_mode (GstPad * pad,
    GstObject * parent, GstPadMode mode, gboolean active);
static GstFlowReturn gst_rs_demuxer_sink_chain (GstPad * pad,
    GstObject * parent, GstBuffer * buf);
static gboolean gst_rs_demuxer_sink_event (GstPad * pad, GstObject * parent,
    GstEvent * event);
static gboolean gst_rs_demuxer_src_query (GstPad * pad, GstObject * parent,
    GstQuery * query);
static gboolean gst_rs_demuxer_src_event (GstPad * pad, GstObject * parent,
    GstEvent * event);
static GstStateChangeReturn gst_rs_demuxer_change_state (GstElement * element,
    GstStateChange transition);
static void gst_rs_demuxer_loop (GstRsDemuxer * demuxer);

static GObjectClass *parent_class;

static void
gst_rs_demuxer_class_init (GstRsDemuxerClass * klass)
{
  GObjectClass *gobject_class;
  GstElementClass *gstelement_class;
  ElementData *data = g_hash_table_lookup (demuxers,
      GSIZE_TO_POINTER (G_TYPE_FROM_CLASS (klass)));
  GstCaps *caps;
  GstPadTemplate *templ;
  g_assert (data != NULL);

  gobject_class = G_OBJECT_CLASS (klass);
  gstelement_class = GST_ELEMENT_CLASS (klass);

  gobject_class->finalize = gst_rs_demuxer_finalize;

  gstelement_class->change_state = gst_rs_demuxer_change_state;

  gst_element_class_set_static_metadata (gstelement_class,
      data->long_name, data->classification, data->description, data->author);

  caps = gst_caps_from_string (data->input_format);
  templ = gst_pad_template_new ("sink", GST_PAD_SINK, GST_PAD_ALWAYS, caps);
  gst_caps_unref (caps);

  gst_element_class_add_pad_template (gstelement_class, templ);

  caps = gst_caps_from_string (data->output_formats);
  templ = gst_pad_template_new ("src_%u", GST_PAD_SRC, GST_PAD_SOMETIMES, caps);
  gst_caps_unref (caps);

  gst_element_class_add_pad_template (gstelement_class, templ);
}

static void
gst_rs_demuxer_init (GstRsDemuxer * demuxer, GstRsDemuxerClass * klass)
{
  ElementData *data = g_hash_table_lookup (demuxers,
      GSIZE_TO_POINTER (G_TYPE_FROM_CLASS (klass)));
  GstPadTemplate *templ;
  g_assert (data != NULL);

  demuxer->instance = demuxer_new (demuxer, data->create_instance);

  templ =
      gst_element_class_get_pad_template (GST_ELEMENT_CLASS (klass), "sink");
  demuxer->sinkpad = gst_pad_new_from_template (templ, "sink");

  gst_pad_set_activate_function (demuxer->sinkpad,
      gst_rs_demuxer_sink_activate);
  gst_pad_set_activatemode_function (demuxer->sinkpad,
      gst_rs_demuxer_sink_activate_mode);
  gst_pad_set_chain_function (demuxer->sinkpad, gst_rs_demuxer_sink_chain);
  gst_pad_set_event_function (demuxer->sinkpad, gst_rs_demuxer_sink_event);
  gst_element_add_pad (GST_ELEMENT (demuxer), demuxer->sinkpad);

  demuxer->flow_combiner = gst_flow_combiner_new ();
}

static void
gst_rs_demuxer_finalize (GObject * object)
{
  GstRsDemuxer *demuxer = GST_RS_DEMUXER (object);

  gst_flow_combiner_free (demuxer->flow_combiner);
  demuxer_drop (demuxer->instance);

  G_OBJECT_CLASS (parent_class)->finalize (object);
}

static gboolean
gst_rs_demuxer_sink_activate (GstPad * pad, GstObject * parent)
{
  GstRsDemuxer *demuxer = GST_RS_DEMUXER (parent);
  GstQuery *query;
  GstPadMode mode = GST_PAD_MODE_PUSH;

  query = gst_query_new_scheduling ();
  if (!gst_pad_peer_query (pad, query)) {
    gst_query_unref (query);
    return FALSE;
  }
  // TODO
  //if (gst_query_has_scheduling_mode_with_flags (query, GST_PAD_MODE_PULL, GST_SCHEDULING_FLAG_SEEKABLE))
  //  mode = GST_PAD_MODE_PULL;
  gst_query_unref (query);

  demuxer->upstream_size = -1;
  query = gst_query_new_duration (GST_FORMAT_BYTES);
  if (gst_pad_peer_query (pad, query)) {
    gst_query_parse_duration (query, NULL, &demuxer->upstream_size);
  }
  gst_query_unref (query);

  return gst_pad_activate_mode (pad, mode, TRUE);
}

static gboolean
gst_rs_demuxer_sink_activate_mode (GstPad * pad,
    GstObject * parent, GstPadMode mode, gboolean active)
{
  GstRsDemuxer *demuxer = GST_RS_DEMUXER (parent);
  gboolean res = TRUE;

  if (active) {
    if (!demuxer_start (demuxer->instance, demuxer->upstream_size,
            mode == GST_PAD_MODE_PULL ? TRUE : FALSE)) {
      res = FALSE;
      goto out;
    }
    if (mode == GST_PAD_MODE_PULL)
      gst_pad_start_task (pad, (GstTaskFunction) gst_rs_demuxer_loop, demuxer,
          NULL);
  } else {
    if (mode == GST_PAD_MODE_PULL)
      gst_pad_stop_task (pad);
  }

out:

  return res;
}

static GstFlowReturn
gst_rs_demuxer_sink_chain (GstPad * pad, GstObject * parent, GstBuffer * buf)
{
  GstRsDemuxer *demuxer = GST_RS_DEMUXER (parent);

  return demuxer_handle_buffer (demuxer->instance, buf);
}

static gboolean
gst_rs_demuxer_sink_event (GstPad * pad, GstObject * parent, GstEvent * event)
{
  GstRsDemuxer *demuxer = GST_RS_DEMUXER (parent);
  gboolean res = FALSE;

  switch (GST_EVENT_TYPE (event)) {
    case GST_EVENT_SEGMENT:{
      // TODO
      res = TRUE;
      gst_event_unref (event);
      break;
    }
    case GST_EVENT_EOS:
      demuxer_end_of_stream (demuxer->instance);
      res = gst_pad_event_default (pad, parent, event);
      break;
    default:
      res = gst_pad_event_default (pad, parent, event);
      break;
  }
  return res;
}

static gboolean
gst_rs_demuxer_src_query (GstPad * pad, GstObject * parent, GstQuery * query)
{
  GstRsDemuxer *demuxer = GST_RS_DEMUXER (parent);
  gboolean res = FALSE;

  switch (GST_QUERY_TYPE (query)) {
    case GST_QUERY_POSITION:{
      GstFormat format;

      gst_query_parse_position (query, &format, NULL);
      if (format == GST_FORMAT_TIME) {
        gint64 position;

        if (demuxer_get_position (demuxer->instance, &position)) {
          gst_query_set_position (query, format, position);
          res = TRUE;
        } else {
          res = FALSE;
        }
      }
      break;
    }
    case GST_QUERY_DURATION:{
      GstFormat format;

      gst_query_parse_duration (query, &format, NULL);
      if (format == GST_FORMAT_TIME) {
        gint64 duration;

        if (demuxer_get_duration (demuxer->instance, &duration)) {
          gst_query_set_duration (query, format, duration);
          res = TRUE;
        } else {
          res = FALSE;
        }
      }
      break;
    }
    default:
      res = gst_pad_query_default (pad, parent, query);
      break;
  }
  return res;
}

static gboolean
gst_rs_demuxer_src_event (GstPad * pad, GstObject * parent, GstEvent * event)
{
  GstRsDemuxer *demuxer = GST_RS_DEMUXER (parent);
  gboolean res = FALSE;

  switch (GST_EVENT_TYPE (event)) {
    case GST_EVENT_SEEK:{
      // TODO
      g_assert_not_reached ();
      gst_event_unref (event);
      break;
    }
    default:
      res = gst_pad_event_default (pad, parent, event);
      break;
  }
  return res;
}

static GstStateChangeReturn
gst_rs_demuxer_change_state (GstElement * element, GstStateChange transition)
{
  GstRsDemuxer *demuxer = GST_RS_DEMUXER (element);
  GstStateChangeReturn result;

  switch (transition) {
    case GST_STATE_CHANGE_READY_TO_PAUSED:
      demuxer->offset = 0;
      gst_segment_init (&demuxer->segment, GST_FORMAT_TIME);
      demuxer->group_id = gst_util_group_id_next ();
      demuxer->segment_seqnum = gst_util_seqnum_next ();

      break;
    default:
      break;
  }

  if (result == GST_STATE_CHANGE_FAILURE)
    return result;
  result = GST_ELEMENT_CLASS (parent_class)->change_state (element, transition);
  if (result == GST_STATE_CHANGE_FAILURE)
    return result;

  switch (transition) {
    case GST_STATE_CHANGE_PAUSED_TO_READY:{
      guint i;

      /* Ignore stop failures */
      demuxer_stop (demuxer->instance);

      gst_flow_combiner_clear (demuxer->flow_combiner);

      for (i = 0; i < demuxer->n_srcpads; i++)
        gst_element_remove_pad (GST_ELEMENT (demuxer), demuxer->srcpads[i]);
      memset (demuxer->srcpads, 0, sizeof (demuxer->srcpads));
      break;
    }
    default:
      break;
  }

  return result;
}

static void
gst_rs_demuxer_loop (GstRsDemuxer * demuxer)
{
  // TODO
  g_assert_not_reached ();
}

void
gst_rs_demuxer_add_stream (GstRsDemuxer * demuxer, guint32 index,
    const gchar * format, const gchar * stream_id)
{
  GstPad *pad;
  GstPadTemplate *templ;
  GstCaps *caps;
  GstEvent *event;
  gchar *name, *full_stream_id;

  g_assert (demuxer->srcpads[index] == NULL);
  g_assert (demuxer->n_srcpads == index);

  templ =
      gst_element_class_get_pad_template (GST_ELEMENT_GET_CLASS (demuxer),
      "src_%u");

  name = g_strdup_printf ("src_%u", index);
  pad = demuxer->srcpads[index] = gst_pad_new_from_template (templ, name);
  g_free (name);

  gst_pad_set_query_function (pad, gst_rs_demuxer_src_query);
  gst_pad_set_event_function (pad, gst_rs_demuxer_src_event);

  gst_pad_set_active (pad, TRUE);

  full_stream_id =
      gst_pad_create_stream_id (pad, GST_ELEMENT (demuxer), stream_id);
  event = gst_event_new_stream_start (full_stream_id);
  gst_event_set_group_id (event, demuxer->group_id);
  g_free (full_stream_id);
  gst_pad_push_event (pad, event);

  caps = gst_caps_from_string (format);
  event = gst_event_new_caps (caps);
  gst_caps_unref (caps);
  gst_pad_push_event (pad, event);

  event = gst_event_new_segment (&demuxer->segment);
  gst_event_set_seqnum (event, demuxer->segment_seqnum);
  gst_pad_push_event (pad, event);

  gst_flow_combiner_add_pad (demuxer->flow_combiner, pad);
  gst_element_add_pad (GST_ELEMENT (demuxer), pad);
  demuxer->n_srcpads++;
}

void
gst_rs_demuxer_added_all_streams (GstRsDemuxer * demuxer)
{
  gst_element_no_more_pads (GST_ELEMENT (demuxer));
  demuxer->group_id = gst_util_group_id_next ();
}

void
gst_rs_demuxer_stream_format_changed (GstRsDemuxer * demuxer, guint32 index,
    const gchar * format)
{
  GstCaps *caps;
  GstEvent *event;

  g_assert (demuxer->srcpads[index] != NULL);

  caps = gst_caps_from_string (format);
  event = gst_event_new_caps (caps);
  gst_caps_unref (caps);

  gst_pad_push_event (demuxer->srcpads[index], event);
}

void
gst_rs_demuxer_stream_eos (GstRsDemuxer * demuxer, guint32 index)
{
  GstCaps *caps;
  GstEvent *event;

  g_assert (demuxer->srcpads[index] != NULL);

  event = gst_event_new_eos ();
  if (index == -1) {
    gint i;

    for (i = 0; i < demuxer->n_srcpads; i++)
      gst_pad_push_event (demuxer->srcpads[i], gst_event_ref (event));

    gst_event_unref (event);
  } else {
    gst_pad_push_event (demuxer->srcpads[index], event);
  }
}


GstFlowReturn
gst_rs_demuxer_stream_push_buffer (GstRsDemuxer * demuxer, guint32 index,
    GstBuffer * buffer)
{
  GstFlowReturn res = GST_FLOW_OK;

  g_assert (demuxer->srcpads[index] != NULL);

  res = gst_pad_push (demuxer->srcpads[index], buffer);
  res = gst_flow_combiner_update_flow (demuxer->flow_combiner, res);

  return res;
}

void
gst_rs_demuxer_remove_all_streams (GstRsDemuxer * demuxer)
{
  guint i;

  gst_flow_combiner_clear (demuxer->flow_combiner);

  for (i = 0; i < demuxer->n_srcpads; i++)
    gst_element_remove_pad (GST_ELEMENT (demuxer), demuxer->srcpads[i]);
  memset (demuxer->srcpads, 0, sizeof (demuxer->srcpads));
}

gboolean
gst_rs_demuxer_plugin_init (GstPlugin * plugin)
{
  demuxers = g_hash_table_new (g_direct_hash, g_direct_equal);
  GST_DEBUG_CATEGORY_INIT (gst_rs_demuxer_debug, "rsdemux", 0,
      "rsdemux element");

  parent_class = g_type_class_ref (GST_TYPE_ELEMENT);

  return demuxers_register (plugin);
}

gboolean
gst_rs_demuxer_register (GstPlugin * plugin, const gchar * name,
    const gchar * long_name, const gchar * description,
    const gchar * classification, const gchar * author, GstRank rank,
    void *create_instance, const gchar * input_format,
    const gchar * output_formats)
{
  GTypeInfo type_info = {
    sizeof (GstRsDemuxerClass),
    NULL,
    NULL,
    (GClassInitFunc) gst_rs_demuxer_class_init,
    NULL,
    NULL,
    sizeof (GstRsDemuxer),
    0,
    (GInstanceInitFunc) gst_rs_demuxer_init
  };
  GType type;
  gchar *type_name;
  ElementData *data;

  data = g_new0 (ElementData, 1);
  data->long_name = g_strdup (long_name);
  data->description = g_strdup (description);
  data->classification = g_strdup (classification);
  data->author = g_strdup (author);
  data->create_instance = create_instance;
  data->input_format = g_strdup (input_format);
  data->output_formats = g_strdup (output_formats);

  type_name = g_strconcat ("RsDemuxer-", name, NULL);
  type = g_type_register_static (GST_TYPE_ELEMENT, type_name, &type_info, 0);
  g_free (type_name);

  g_hash_table_insert (demuxers, GSIZE_TO_POINTER (type), data);

  if (!gst_element_register (plugin, name, rank, type))
    return FALSE;

  return TRUE;
}
