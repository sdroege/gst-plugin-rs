# webrtcsink

All-batteries included GStreamer WebRTC producer, that tries its best to do The Right Thing™.

## Use case

The [webrtcbin] element in GStreamer is extremely flexible and powerful, but using
it can be a difficult exercise. When all you want to do is serve a fixed set of streams
to any number of consumers, `webrtcsink` (which wraps `webrtcbin` internally) can be a
useful alternative.

[webrtcbin]: https://gstreamer.freedesktop.org/documentation/webrtc/index.html

## Features

`webrtcsink` implements the following features:

* Built-in signaller: when using the default signalling server (provided as a python
  script [here](signalling/simple-server.py)), this element will perform signalling without
  requiring application interaction. This makes it usable directly from `gst-launch`.

* Application-provided signalling: `webrtcsink` can be instantiated by an application
  with a custom signaller. That signaller must be a GObject, and must implement the
  `Signallable` interface as defined [here](plugins/src/webrtcsink/mod.rs). The
  [default signaller](plugins/src/signaller/mod.rs) can be used as an example.

* Sandboxed consumers: when a consumer is added, its encoder / payloader / webrtcbin
  elements run in a separately managed pipeline. This provides a certain level of
  sandboxing, as opposed to having those elements running inside the element itself.

  It is important to note that at this moment, encoding is not shared between consumers.
  While this is not on the roadmap at the moment, nothing in the design prevents
  implementing this optimization.

* Configuration: the level of user control over the element is at the moment quite
  narrow, as the only interface exposed is control over proposed codecs, as well
  as their order of priority. Consult `gst-inspect=1.0` for more information.

More features are on the roadmap, focusing on mechanisms for mitigating packet
loss and congestion.

It is important to note that full control over the individual elements used by
`webrtcsink` is *not* on the roadmap, as it will act as a black box in that respect,
for example `webrtcsink` wants to reserve control over the bitrate for congestion
control.

If more granular control is required, applications should use `webrtcbin` directly,
`webrtcsink` will focus on trying to just do the right thing, although it might
expose interfaces to guide and tune the heuristics it employs.

## Building

### Prerequisites

The element has only been tested for now against GStreamer master, with an
extra Merge Request pending review:

<https://gitlab.freedesktop.org/gstreamer/gstreamer/-/merge_requests/1233>

The MR should hopefully make it in in time for GStreamer's 1.20 release,
in the meantime the patches must be applied locally.

For testing, it is recommended to simply build GStreamer locally and run
in the uninstalled devenv.

> Make sure to install the development packages for some codec libraries
> beforehand, such as libx264, libvpx and libopusenc, exact names depend
> on your distribution.

```
git clone https://gitlab.freedesktop.org/meh/gstreamer/-/tree/webrtcsink
meson build
ninja -C build
ninja -C build devenv
```

### Compiling

``` shell
cargo build
```

## Usage

Open three terminals. In the first, run:

``` shell
cd signalling
python3 simple-server.py --addr=127.0.0.1 --disable-ssl
```

In the second, run:

``` shell
cd www
python3 -m http.server
```

In the third, run:

```
export GST_PLUGIN_PATH=$PWD/target/debug:$GST_PLUGIN_PATH
gst-launch-1.0 webrtcsink name=ws videotestsrc ! ws. audiotestsrc ! ws.
```

When the pipeline above is running succesfully, open a browser and
point it to the python server:

```
xdg-open http://127.0.0.1:8000
```

You should see an identifier listed in the left-hand panel, click on
it. You should see a test video stream, and hear a test tone.

## License

All code in this repository is licensed under the [MIT license].

[MIT license]: https://opensource.org/licenses/MIT
