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

* Built-in signaller: when using the default signalling server, this element will
  perform signalling without requiring application interaction.
  This makes it usable directly from `gst-launch`.

* Application-provided signalling: `webrtcsink` can be instantiated by an application
  with a custom signaller. That signaller must be a GObject, and must implement the
  `Signallable` interface as defined [here](plugins/src/webrtcsink/mod.rs). The
  [default signaller](plugins/src/signaller/mod.rs) can be used as an example.

  An [example project] is also available to use as a boilerplate for implementing
  and using a custom signaller.

* Sandboxed consumers: when a consumer is added, its encoder / payloader / webrtcbin
  elements run in a separately managed pipeline. This provides a certain level of
  sandboxing, as opposed to having those elements running inside the element itself.

  It is important to note that at this moment, encoding is not shared between consumers.
  While this is not on the roadmap at the moment, nothing in the design prevents
  implementing this optimization.

* Congestion control: the element leverages transport-wide congestion control
  feedback messages in order to adapt the bitrate of individual consumers' video
  encoders to the available bandwidth.

* Configuration: the level of user control over the element is slowly expanding,
  consult `gst-inspect-1.0` for more information on the available properties and
  signals.

* Packet loss mitigation: webrtcsink now supports sending protection packets for
  Forward Error Correction, modulating the amount as a function of the available
  bandwidth, and can honor retransmission requests. Both features can be disabled
  via properties.

It is important to note that full control over the individual elements used by
`webrtcsink` is *not* on the roadmap, as it will act as a black box in that respect,
for example `webrtcsink` wants to reserve control over the bitrate for congestion
control.

A signal is now available however for the application to provide the initial
configuration for the encoders `webrtcsink` instantiates.

If more granular control is required, applications should use `webrtcbin` directly,
`webrtcsink` will focus on trying to just do the right thing, although it might
expose more interfaces to guide and tune the heuristics it employs.

[example project]: https://github.com/centricular/webrtcsink-custom-signaller

## Building

### Prerequisites

The element has only been tested for now against GStreamer main.

For testing, it is recommended to simply build GStreamer locally and run
in the uninstalled devenv.

> Make sure to install the development packages for some codec libraries
> beforehand, such as libx264, libvpx and libopusenc, exact names depend
> on your distribution.

```
git clone --depth 1 --single-branch --branch main https://gitlab.freedesktop.org/gstreamer/gstreamer 
cd gstreamer
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
WEBRTCSINK_SIGNALLING_SERVER_LOG=debug cargo run --bin server
```

In the second, run:

``` shell
python3 -m http.server -d www/
```

In the third, run:

``` shell
export GST_PLUGIN_PATH=$PWD/target/debug:$GST_PLUGIN_PATH
gst-launch-1.0 webrtcsink name=ws videotestsrc ! ws. audiotestsrc ! ws.
```

When the pipeline above is running succesfully, open a browser and
point it to the http server:

``` shell
xdg-open http://127.0.0.1:8000
```

You should see an identifier listed in the left-hand panel, click on
it. You should see a test video stream, and hear a test tone.

## Configuration

The element itself can be configured through its properties, see
`gst-inspect-1.0 webrtcsink` for more information about that, in addition the
default signaller also exposes properties for configuring it, in
particular setting the signalling server address, those properties
can be accessed through the `gst::ChildProxy` interface, for example
with gst-launch:

``` shell
gst-launch-1.0 webrtcsink signaller::address="ws://127.0.0.1:8443" ..
```

The signaller object can not be inspected, refer to [the source code]
for the list of properties.

[the source code]: plugins/src/signaller/imp.rs


### Enable 'navigation' a.k.a user interactivity with the content

`webrtcsink` implements the [`GstNavigation`] interface which allows interacting
with the content, for example move with your mouse, entering keys with the
keyboard, etc... On top of that a `WebRTCDataChannel` based protocol has been
implemented and can be activated with the `enable-data-channel-navigation=true`
property. The [demo](www/) implements the protocol and you can easily test this
feature, using the [`wpesrc`] for example.

As an example, the following pipeline allows you to navigate the GStreamer
documentation inside the video running within your web browser (in
http://127.0.0.1:8000 if you followed previous steps of that readme):

```
gst-launch-1.0 wpesrc location=https://gstreamer.freedesktop.org/documentation/ ! webrtcsink enable-data-channel-navigation=true
```

[`GstNavigation`]: https://gstreamer.freedesktop.org/documentation/video/gstnavigation.html
[`wpesrc`]: https://gstreamer.freedesktop.org/documentation/wpe/wpesrc.html

## Testing congestion control

For the purpose of testing congestion in a reproducible manner, a
[simple tool] has been used, I only used it on Linux but it is documented
as usable on MacOS too. I had to run the client browser on a separate
machine on my local network for congestion to actually be applied, I didn't
look into why that was necessary.

My testing procedure was:

* identify the server machine network interface (eg with `ifconfig` on Linux)

* identify the client machine IP address (eg with `ifconfig` on Linux)

* start the various services as explained in the Usage section (use
  `GST_DEBUG=webrtcsink:7` to get detailed logs about congestion control)

* start playback in the client browser

* Run a `comcast` command on the server machine, for instance:

  ``` shell
  /home/meh/go/bin/comcast --device=$SERVER_INTERFACE --target-bw 3000 --target-addr=$CLIENT_IP --target-port=1:65535 --target-proto=udp
  ```

* Observe the bitrate sharply decreasing, playback should slow down briefly
  then catch back up

* Remove the bandwidth limitation, and observe the bitrate eventually increasing
  back to a maximum:

  ``` shell
  /home/meh/go/bin/comcast --device=$SERVER_INTERFACE --stop
  ```

For comparison, the congestion control property can be set to disabled on
webrtcsink, then the above procedure applied again, the expected result is
for playback to simply crawl down to a halt until the bandwidth limitation
is lifted:

``` shell
gst-launch-1.0 webrtcsink congestion-control=disabled
```

[simple tool]: https://github.com/tylertreat/comcast

## Monitoring tool

An example server / client application for monitoring per-consumer stats
can be found [here].

[here]: plugins/examples/README.md

## License

All code in this repository is licensed under the [MIT license].

[MIT license]: https://opensource.org/licenses/MIT
