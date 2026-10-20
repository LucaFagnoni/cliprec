#!/usr/bin/env bash
# Build FFmpeg ridotta (LGPL 3) per ClipRec: solo i componenti usati da cliprec-encode e cliprec-mux.
# Si lancia da `cargo xtask ffmpeg-ridotta` nella shell UCRT64 di MSYS2 (third_party/toolchain/msys64).
# Uso: ffmpeg-ridotta.sh <sorgente FFmpeg> <prefisso di installazione>
set -euo pipefail
src=$(cygpath -u "$1")
prefix=$(cygpath -u "$2")
cd "$src"

# Le librerie esterne (svt-av1, opus, lame, libvpl) e il runtime di gcc entrano staticamente nelle DLL:
# la distribuzione resta di quattro DLL (avcodec, avformat, avutil, swresample) senza dipendenze MinGW.
# libvpl (dispatcher QSV) è C++ e il suo .pc non dichiara libstdc++: la si aggiunge a mano.
./configure \
  --prefix="$prefix" \
  --arch=x86_64 --target-os=mingw32 \
  --pkg-config-flags=--static \
  --extra-ldflags=-static --extra-libs=-lstdc++ \
  --extra-version=cliprec \
  --enable-shared --disable-static \
  --enable-version3 \
  --disable-debug --disable-doc --disable-programs --disable-network \
  --disable-autodetect --enable-w32threads \
  --disable-everything \
  --disable-avfilter --disable-avdevice --disable-swscale \
  --enable-encoder=h264_nvenc,hevc_nvenc,av1_nvenc \
  --enable-encoder=h264_qsv,hevc_qsv,av1_qsv \
  --enable-encoder=h264_amf,hevc_amf,av1_amf \
  --enable-encoder=libsvtav1,libopus,libmp3lame,aac,flac,pcm_s24le \
  --enable-muxer=mp4,matroska,flac,wav,opus,mp3 \
  --enable-bsf=aac_adtstoasc \
  --enable-parser=hevc \
  --enable-protocol=file \
  --enable-ffnvcodec --enable-nvenc --enable-amf --enable-libvpl \
  --enable-d3d11va \
  --enable-libsvtav1 --enable-libopus --enable-libmp3lame

make -j"$(nproc)"
make install

# Come la build BtbN: librerie di importazione MSVC in lib/, testo LGPL 3 in LICENSE.txt.
mv "$prefix"/bin/*.lib "$prefix/lib/"
cp COPYING.LGPLv3 "$prefix/LICENSE.txt"
# Senza programmi non c'è `ffmpeg -version`: stesse righe ricavate da config.h e ffversion.h,
# lette da `cargo xtask dist` (FFmpeg-BUILD.txt) e `cargo xtask notices`.
define() { sed -n "s/^#define $1 \"\(.*\)\"$/\1/p" "$2"; }
{
  echo "ffmpeg version $(define FFMPEG_VERSION libavutil/ffversion.h) Copyright (c) 2000-2026 the FFmpeg developers"
  echo "built with $(define CC_IDENT config.h)"
  echo "configuration: $(define FFMPEG_CONFIGURATION config.h)"
} > "$prefix/ffmpeg-version.txt"
