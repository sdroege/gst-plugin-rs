# gst-plugins-spotify

This is a [GStreamer](https://gstreamer.freedesktop.org/) plugin to read content from
[Spotify](https://www.spotify.com/).

Make sure that your application follows [Spotify's design guidelines](https://developer.spotify.com/documentation/general/design-and-branding/)
to respect their legal/licensing restrictions.

## Spotify Credentials

This plugin requires a [Spotify Premium](https://www.spotify.com/premium/) account.
If your account is linked with Facebook, you'll need to setup
a [device username and password](https://www.spotify.com/us/account/set-device-password/).

Those username and password are then set using the `username` and `password` properties.

You may also want to cache credentials and downloaded files, see the `cache-` properties on the element.

## spotifyaudiosrc

The `spotifyaudiosrc` element can be used to play a song from Spotify using its [Spotify URI](https://community.spotify.com/t5/FAQs/What-s-a-Spotify-URI/ta-p/919201).

```
gst-launch-1.0 spotifyaudiosrc username=$USERNAME password=$PASSWORD track=spotify:track:3i3P1mGpV9eRlfKccjDjwi ! oggdemux ! vorbisdec ! audioconvert ! autoaudiosink
```

The element also implements an URI handler which accepts credentials and cache settings as URI parameters:

```console
gst-launch-1.0 playbin3 uri=spotify:track:3i3P1mGpV9eRlfKccjDjwi?username=$USERNAME\&password=$PASSWORD\&cache-credentials=cache\&cache-files=cache
```