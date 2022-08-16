# webrtcsink examples

Collection (1-sized for now) of webrtcsink examples

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
