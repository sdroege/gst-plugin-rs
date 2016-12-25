all:
	cd gst-plugin && cargo build
	cd gst-plugin-file && cargo build
	cd gst-plugin-http && cargo build
	cd gst-plugin-flv && cargo build

clean:
	cargo clean

