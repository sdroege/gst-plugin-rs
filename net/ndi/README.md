GStreamer NDI Plugin
====================

*Compatible with NDI SDK 5.x*

This is a plugin for the [GStreamer](https://gstreamer.freedesktop.org/)
multimedia framework that allows GStreamer to receive or send an
[NDI](https://www.newtek.com/ndi/) stream.

This plugin has been initially developed by [Teltek](http://teltek.es/) and
was funded by the [University of the Arts London](https://www.arts.ac.uk/) and
[The University of Manchester](https://www.manchester.ac.uk/).

Currently the plugin has a source element for receiving from NDI sources, a
sink element to provide an NDI source and a device provider for discovering
NDI sources on the network.

The plugin is loading the NDI SDK at runtime, either from the default library
path or, if set, from the directory given by the `NDI_RUNTIME_DIR_V5`
environment variable.

Some examples of how to use these elements from the command line:

```console
# Information about the elements
$ gst-inspect-1.0 ndi
$ gst-inspect-1.0 ndisrc
$ gst-inspect-1.0 ndisink

# Discover all NDI sources on the network
$ gst-device-monitor-1.0 -f Source/Network:application/x-ndi

# Audio/Video source pipeline
$ gst-launch-1.0 ndisrc ndi-name="GC-DEV2 (OBS)" ! ndisrcdemux name=demux   demux.video ! queue ! videoconvert ! autovideosink  demux.audio ! queue ! audioconvert ! autoaudiosink

# Audio/Video sink pipeline
$ gst-launch-1.0 videotestsrc is-live=true ! video/x-raw,format=UYVY ! ndisinkcombiner name=combiner ! ndisink ndi-name="My NDI source"  audiotestsrc is-live=true ! combiner.audio
```

Feel free to contribute to this project. Some ways you can contribute are:

* Testing with more hardware and software and reporting bugs
* Doing pull requests.

License
-------
This plugin is licensed under the MPL-2 - see the [LICENSE](LICENSE-MPL-2.0) file for details

Acknowledgments
-------
* University of the Arts London and The University of Manchester.
* Sebastian Dröge (@sdroege).
