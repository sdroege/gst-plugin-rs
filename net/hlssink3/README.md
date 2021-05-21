# GStreamer HTTP Live Streaming Plugin
A GStreamer HLS sink plugin. Based on the
["hlssink2"](https://gstreamer.freedesktop.org/documentation/hls/hlssink2.html?gi-language=c)
element.

The "hlssink3" is feature-equivalent to the "hlssink2" element. Any pipeline that uses "hlssink2" can use the
"hlssink3" element and the result should be the same.

The "hlssink3" element has a `playlist-type` property used to control the behavior of the HLS playlist file. The
available values for this property are:
- `null` (default): The tag `#EXT-X-PLAYLIST-TYPE` won't be present in the playlist during the pipeline processing. The
  playlist will be updated in sync as new segments are available, old segments are removed, keeping N segments as
  defined in the property `playlist-length`. This is the default behavior, and is compatible with how "hlssink2" works;
- `"event"`: The playlist is updated as new segments are available, and the tag `#EXT-X-PLAYLIST-TYPE:EVENT` is present 
  during processing. No segments will be removed from the playlist. At the end of the processing, the tag
  `#EXT-X-ENDLIST` is added to the playlist;
- `"vod"`: The playlist behaves like the `event` option (a live event), but at the end of the processing, the playlist 
  will be set to `#EXT-X-PLAYLIST-TYPE:VOD`.
