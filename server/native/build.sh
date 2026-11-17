#!/bin/sh
# Build the scryjpeg companion shim (LANE B unit 6b).
#
# scrybox-server dlopens this for JPEG decodes so they are BIT-IDENTICAL to
# the container's sharp/libvips/mozjpeg (libvips jpeg2vips.c uses the raw
# libjpeg API exactly like scryjpeg.c; the pure-Rust image crate decoder
# drifts LSBs on baseline JPEGs — jdcoefct CMISS handling — and the 448x448
# embed tensor amplifies it).
#
# mozjpeg 4.1.5 is the version libvips 8.15.3 / sharp 0.33.5 ship in the
# container — pin it, do not "upgrade" the shim.
#   tarball: https://github.com/mozilla/mozjpeg/archive/refs/tags/v4.1.5.tar.gz
#   (the repo now lives under the mozilla org; libjpeg-turbo/mozjpeg 404s)
#   cmake>=3.10,<4 and ninja: pip install "cmake<4" ninja
#
# usage: ./build.sh <path-to-mozjpeg-4.1.5-source> <out-dir>
#   then point the lane at it:  SCRYBOX_JPEG_LIB=<out-dir>/libscryjpeg.so
#   (restart-rust-b.sh defaults to <repo-root>/data/lib/libscryjpeg.so)
set -eu
MOZ=${1:?mozjpeg 4.1.5 source dir}
OUT=${2:?output dir}
mkdir -p "$OUT"
cmake -S "$MOZ" -B "$MOZ/build" -DCMAKE_BUILD_TYPE=Release \
  -DENABLE_SHARED=1 -DENABLE_STATIC=1 \
  -DCMAKE_POSITION_INDEPENDENT_CODE=ON -DPNG_SUPPORTED=0
cmake --build "$MOZ/build" --config Release
gcc -O2 -fPIC -shared -I"$MOZ" -I"$MOZ/build" scryjpeg.c "$MOZ/build/libjpeg.a" -lm \
  -o "$OUT/libscryjpeg.so"
echo "built $OUT/libscryjpeg.so (mozjpeg $MOZ static-linked)"
