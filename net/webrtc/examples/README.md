# webrtcsink examples

Collection of webrtcsink examples

## webrtcsink-stats-server

A simple application that instantiates a webrtcsink and serves stats
over websockets.

The application expects a signalling server to be running at `ws://localhost:8443`,
similar to the usage example in the main README.

``` shell
cargo run --example webrtcsink-stats-server
```

Once it is running, follow the instruction in the webrtcsink-stats folder to
run an example client.

## webrtcsink-custom-signaller

An example of custom signaller implementation, see the corresponding
[README](webrtcsink-custom-signaller/README.md) for more details on code and usage. 
