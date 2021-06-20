set -e

RELEASE=0.9.0

git clone https://code.videolan.org/videolan/dav1d.git --branch $RELEASE
cd dav1d
meson build -D prefix=/usr/local
ninja -C build
ninja -C build install
cd ..
