ARG RUST_VERSION=1.54
ARG DEBIAN_VERSION=bullseye
ARG GST_PLUGINS_RS_VERSION=0.7.1

FROM rust:${RUST_VERSION}-${DEBIAN_VERSION} as builder

# https://ryandaniels.ca/blog/docker-dockerfile-arg-from-arg-trouble/
ARG GST_PLUGINS_RS_VERSION

WORKDIR /usr/src/gst-plugins-rs

ENV CSOUND_LIB_DIR /usr/lib/x86_64-linux-gnu/
ENV PLUGINS_DIR /opt/gst-plugins-rs
ENV CARGO_PROFILE_RELEASE_DEBUG false

RUN apt update \
    && apt install -yq --no-install-recommends \
	libgstreamer-plugins-base1.0-dev \
	libgstreamer1.0-dev \
        libcsound64-dev \
	libclang-11-dev \
 	libpango1.0-dev  \
	libdav1d-dev 

COPY . .

RUN make && \
    make install

FROM scratch as release

COPY --from=builder /opt/gst-plugins-rs /usr/lib/x86_64-linux-gnu/gstreamer-1.0
