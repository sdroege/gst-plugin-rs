/* Copyright (C) 2016-2017 Sebastian Dröge <sebastian@centricular.com>
 *
 * Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
 * http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
 * <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
 * option. This file may not be copied, modified, or distributed
 * except according to those terms.
 */

#include "demuxer.h"

#include <string.h>
#include <stdint.h>

typedef struct
{
  gchar *long_name;
  gchar *description;
  gchar *classification;
  gchar *author;
  void *create_instance;
  GstCaps *input_caps;
  GstCaps *output_caps;
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
  GstPadTemplate *templ;
  g_assert (data != NULL);

  gobject_class = G_OBJECT_CLASS (klass);
  gstelement_class = GST_ELEMENT_CLASS (klass);

  gobject_class->finalize = gst_rs_demuxer_finalize;

  gstelement_class->change_state = gst_rs_demuxer_change_state;

  gst_element_class_set_static_metadata (gstelement_class,
      data->long_name, data->classification, data->description, data->author);

  templ =
      gst_pad_template_new ("sink", GST_PAD_SINK, GST_PAD_ALWAYS,
      data->input_caps);
  gst_element_class_add_pad_template (gstelement_class, templ);

  templ =
      gst_pad_template_new ("src_%u", GST_PAD_SRC, GST_PAD_SOMETIMES,
      data->output_caps);
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

  GST_DEBUG_OBJECT (demuxer, "Instantiating");
}

static void
gst_rs_demuxer_finalize (GObject * object)
{
  GstRsDemuxer *demuxer = GST_RS_DEMUXER (object);

  GST_DEBUG_OBJECT (demuxer, "Finalizing");
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
  //if (gst_query_has_scheduling_mode_with_flags (query, GST_PAD_MODE_PULL, GST_SCHEDULING_FLAG_SEEKABLE)) {
  //  GST_DEBUG_OBJECT (demuxer, "Activating in PULL mode");
  //  mode = GST_PAD_MODE_PULL;
  //} else {
  //GST_DEBUG_OBJECT (demuxer, "Activating in PUSH mode");
  //}
  gst_query_unref (query);

  demuxer->upstream_size = -1;
  query = gst_query_new_duration (GST_FORMAT_BYTES);
  if (gst_pad_peer_query (pad, query)) {
    gst_query_parse_duration (query, NULL, (gint64 *) &demuxer->upstream_size);
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

  GST_DEBUG_OBJECT (demuxer, "%s pad in %s mode",
      (active ? "Activating" : "Deactivating"), gst_pad_mode_get_name (mode));

  if (active) {
    GST_DEBUG_OBJECT (demuxer, "Starting");
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
gst_rs_demuxer_sink_chain (G_GNUC_UNUSED GstPad * pad, GstObject * parent, GstBuffer * buf)
{
  GstRsDemuxer *demuxer = GST_RS_DEMUXER (parent);
  GstFlowReturn res;

  GST_TRACE_OBJECT (demuxer, "Handling buffer %p", buf);

  res = demuxer_handle_buffer (demuxer->instance, buf);

  GST_TRACE_OBJECT (demuxer, "Handling buffer returned %s",
      gst_flow_get_name (res));

  return res;
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
      GST_DEBUG_OBJECT (demuxer, "Got EOS");
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

        if (demuxer_get_position (demuxer->instance, (guint64 *) &position)) {
          GST_DEBUG_OBJECT (demuxer, "Returning position %" GST_TIME_FORMAT,
              GST_TIME_ARGS (position));
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

        if (demuxer_get_duration (demuxer->instance, (guint64 *) &duration)) {
          GST_DEBUG_OBJECT (demuxer, "Returning duration %" GST_TIME_FORMAT,
              GST_TIME_ARGS (duration));
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
  G_GNUC_UNUSED GstRsDemuxer *demuxer = GST_RS_DEMUXER (parent);
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
  GstStateChangeReturn result = GST_STATE_CHANGE_SUCCESS;

  GST_DEBUG_OBJECT (demuxer, "Change state %s to %s",
      gst_element_state_get_name (GST_STATE_TRANSITION_CURRENT (transition)),
      gst_element_state_get_name (GST_STATE_TRANSITION_NEXT (transition)));

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
      GST_DEBUG_OBJECT (demuxer, "Stopping");
      demuxer_stop (demuxer->instance);

      gst_flow_combiner_clear (demuxer->flow_combiner);

      for (i = 0; i < G_N_ELEMENTS (demuxer->srcpads); i++) {
        if (demuxer->srcpads[i])
          gst_element_remove_pad (GST_ELEMENT (demuxer), demuxer->srcpads[i]);
      }
      memset (demuxer->srcpads, 0, sizeof (demuxer->srcpads));
      break;
    }
    default:
      break;
  }

  return result;
}

static void
gst_rs_demuxer_loop (G_GNUC_UNUSED GstRsDemuxer * demuxer)
{
  // TODO
  g_assert_not_reached ();
}

void
gst_rs_demuxer_add_stream (GstRsDemuxer * demuxer, guint32 index,
    GstCaps * caps, const gchar * stream_id)
{
  GstPad *pad;
  GstPadTemplate *templ;
  GstEvent *event;
  gchar *name, *full_stream_id;

  g_assert (demuxer->srcpads[index] == NULL);

  GST_DEBUG_OBJECT (demuxer,
      "Adding stream %u with format %" GST_PTR_FORMAT " and stream id %s",
      index, caps, stream_id);

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

  event = gst_event_new_caps (caps);
  gst_pad_push_event (pad, event);

  event = gst_event_new_segment (&demuxer->segment);
  gst_event_set_seqnum (event, demuxer->segment_seqnum);
  gst_pad_push_event (pad, event);

  gst_flow_combiner_add_pad (demuxer->flow_combiner, pad);
  gst_element_add_pad (GST_ELEMENT (demuxer), pad);
}

void
gst_rs_demuxer_added_all_streams (GstRsDemuxer * demuxer)
{
  GST_DEBUG_OBJECT (demuxer, "No more pads");

  gst_element_no_more_pads (GST_ELEMENT (demuxer));
  demuxer->group_id = gst_util_group_id_next ();
}

void
gst_rs_demuxer_stream_format_changed (GstRsDemuxer * demuxer, guint32 index,
    GstCaps * caps)
{
  GstEvent *event;

  g_assert (demuxer->srcpads[index] != NULL);

  GST_DEBUG_OBJECT (demuxer, "Format changed for stream %u: %" GST_PTR_FORMAT,
      index, caps);

  event = gst_event_new_caps (caps);

  gst_pad_push_event (demuxer->srcpads[index], event);
}

void
gst_rs_demuxer_stream_eos (GstRsDemuxer * demuxer, guint32 index)
{
  GstEvent *event;

  g_assert (index == (guint32) -1 || demuxer->srcpads[index] != NULL);

  GST_DEBUG_OBJECT (demuxer, "EOS for stream %u", index);

  event = gst_event_new_eos ();
  if (index == (guint32) -1) {
    guint i;

    for (i = 0; i < G_N_ELEMENTS (demuxer->srcpads); i++) {
      if (demuxer->srcpads[i])
        gst_pad_push_event (demuxer->srcpads[i], gst_event_ref (event));
    }

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

  GST_DEBUG_OBJECT (demuxer, "Pushing buffer %p for pad %u", buffer, index);
  res = gst_pad_push (demuxer->srcpads[index], buffer);
  GST_DEBUG_OBJECT (demuxer, "Pushed buffer returned: %s",
      gst_flow_get_name (res));
  res = gst_flow_combiner_update_flow (demuxer->flow_combiner, res);
  GST_DEBUG_OBJECT (demuxer, "Combined return: %s", gst_flow_get_name (res));

  return res;
}

void
gst_rs_demuxer_remove_all_streams (GstRsDemuxer * demuxer)
{
  guint i;

  GST_DEBUG_OBJECT (demuxer, "Removing all streams");
  gst_flow_combiner_clear (demuxer->flow_combiner);

  for (i = 0; i < G_N_ELEMENTS (demuxer->srcpads); i++) {
    if (demuxer->srcpads[i])
      gst_element_remove_pad (GST_ELEMENT (demuxer), demuxer->srcpads[i]);
  }
  memset (demuxer->srcpads, 0, sizeof (demuxer->srcpads));
}

static gpointer
gst_rs_demuxer_init_class (G_GNUC_UNUSED gpointer data)
{
  demuxers = g_hash_table_new (g_direct_hash, g_direct_equal);
  GST_DEBUG_CATEGORY_INIT (gst_rs_demuxer_debug, "rsdemux", 0,
      "Rust demuxer base class");

  parent_class = g_type_class_ref (GST_TYPE_ELEMENT);

  return NULL;
}

gboolean
gst_rs_demuxer_register (GstPlugin * plugin, const gchar * name,
    const gchar * long_name, const gchar * description,
    const gchar * classification, const gchar * author, GstRank rank,
    void *create_instance, GstCaps * input_caps, GstCaps * output_caps)
{
  GOnce gonce = G_ONCE_INIT;
  GTypeInfo type_info = {
    sizeof (GstRsDemuxerClass),
    NULL,
    NULL,
    (GClassInitFunc) gst_rs_demuxer_class_init,
    NULL,
    NULL,
    sizeof (GstRsDemuxer),
    0,
    (GInstanceInitFunc) gst_rs_demuxer_init,
    NULL
  };
  GType type;
  gchar *type_name;
  ElementData *data;

  g_once (&gonce, gst_rs_demuxer_init_class, NULL);

  GST_DEBUG ("Registering for %" GST_PTR_FORMAT ": %s", plugin, name);
  GST_DEBUG ("  long name: %s", long_name);
  GST_DEBUG ("  description: %s", description);
  GST_DEBUG ("  classification: %s", classification);
  GST_DEBUG ("  author: %s", author);
  GST_DEBUG ("  rank: %d", rank);
  GST_DEBUG ("  input caps: %" GST_PTR_FORMAT, input_caps);
  GST_DEBUG ("  output caps: %" GST_PTR_FORMAT, output_caps);

  data = g_new0 (ElementData, 1);
  data->long_name = g_strdup (long_name);
  data->description = g_strdup (description);
  data->classification = g_strdup (classification);
  data->author = g_strdup (author);
  data->create_instance = create_instance;
  data->input_caps = gst_caps_ref (input_caps);
  data->output_caps = gst_caps_ref (output_caps);

  type_name = g_strconcat ("RsDemuxer-", name, NULL);
  type = g_type_register_static (GST_TYPE_ELEMENT, type_name, &type_info, 0);
  g_free (type_name);

  g_hash_table_insert (demuxers, GSIZE_TO_POINTER (type), data);

  if (!gst_element_register (plugin, name, rank, type))
    return FALSE;

  return TRUE;
}
