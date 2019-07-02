PLUGINS_DIR ?= $(shell pkg-config --variable=pluginsdir gstreamer-1.0)

OS=$(shell uname -s)
ifeq ($(OS),Linux)
  SO_SUFFIX=so
else
ifeq ($(OS),Darwin)
  SO_SUFFIX=dylib
else
  # FIXME: Bad hack, how to know we're on Windows?
  SO_SUFFIX=dll
endif
endif

all: debug

debug:
	cargo build --all

release:
	cargo build --all --release

install: debug
	install target/debug/*.$(SO_SUFFIX) $(PLUGINS_DIR)

install-release: release
	install target/release/*.$(SO_SUFFIX) $(PLUGINS_DIR)

clean:
	cargo clean

