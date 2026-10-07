FROM debian:bookworm-slim AS build

ARG FFMPEG_VERSION=8.1.3
ARG OPUS_VERSION=1.5.2

ENV DEBIAN_FRONTEND=noninteractive \
    PREFIX=/opt/win64 \
    PKG_CONFIG_LIBDIR=/opt/win64/lib/pkgconfig

RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        binutils-mingw-w64-x86-64 \
        build-essential \
        gcc-mingw-w64-x86-64-posix \
        g++-mingw-w64-x86-64-posix \
        make \
        nasm \
        pkg-config \
        xz-utils \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY build-cache/ /build/downloads/

RUN cp "downloads/opus-${OPUS_VERSION}.tar.gz" opus.tar.gz \
    && echo "65c1d2f78b9f2fb20082c38cbe47c951ad5839345876e46941612ee87f9a7ce1  opus.tar.gz" | sha256sum -c - \
    && tar -xzf opus.tar.gz \
    && cd "opus-${OPUS_VERSION}" \
    && ./configure \
        --host=x86_64-w64-mingw32 \
        --prefix="$PREFIX" \
        --disable-shared \
        --enable-static \
        --disable-doc \
        --disable-extra-programs \
        CFLAGS="-Os -D_FORTIFY_SOURCE=0 -ffunction-sections -fdata-sections" \
    && make -j2 \
    && make install

RUN PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" pkg-config --modversion opus \
    && PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" pkg-config --static --cflags --libs opus

RUN printf '%s\n' '#include <opus_multistream.h>' 'int main(void) {' \
        '  void (*volatile symbol)(void) = (void (*)(void))opus_multistream_surround_encoder_create;' \
        '  return symbol == 0;' '}' > /tmp/opus-probe.c \
    && x86_64-w64-mingw32-gcc \
        $(PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" pkg-config --static --cflags opus) \
        /tmp/opus-probe.c \
        -o /tmp/opus-probe.exe \
        $(PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" pkg-config --static --libs opus)

RUN cp "downloads/ffmpeg-${FFMPEG_VERSION}.tar.xz" ffmpeg.tar.xz \
    && echo "7138d28c96d9d3e3af4ee3d8cad72741f8ffb40da90c1112235dea3ecd3178a3  ffmpeg.tar.xz" | sha256sum -c - \
    && tar -xf ffmpeg.tar.xz \
    && cd "ffmpeg-${FFMPEG_VERSION}" \
    && PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig" ./configure \
        --prefix="$PREFIX" \
        --arch=x86_64 \
        --target-os=mingw32 \
        --cross-prefix=x86_64-w64-mingw32- \
        --enable-cross-compile \
        --pkg-config=pkg-config \
        --pkg-config-flags=--static \
        --enable-static \
        --disable-shared \
        --disable-everything \
        --disable-gpl \
        --disable-version3 \
        --disable-nonfree \
        --disable-network \
        --disable-autodetect \
        --disable-debug \
        --disable-doc \
        --disable-ffplay \
        --disable-ffprobe \
        --enable-ffmpeg \
        --enable-protocol=file,pipe \
        --enable-demuxer=ogg,wav,mp3,mov,matroska,flac,aac,pcm_s16le \
        --enable-decoder=opus,vorbis,mp3,mp3float,aac,flac,alac,pcm_* \
        --enable-encoder=libopus,pcm_s16le,pcm_f32le \
        --enable-muxer=ogg,pcm_s16le,pcm_f32le,wav,null \
        --enable-parser=opus,vorbis,flac,aac,mpegaudio \
        --enable-filter=aresample,aformat,anull,abuffer,abuffersink \
        --enable-libopus \
        --enable-small \
        --extra-cflags="-Os -I$PREFIX/include -ffunction-sections -fdata-sections" \
        --extra-ldflags="-L$PREFIX/lib -static -static-libgcc -Wl,--gc-sections" \
    && make -j2 \
    && x86_64-w64-mingw32-strip --strip-all ffmpeg.exe \
    && test "$(stat -c %s ffmpeg.exe)" -lt 10000000 \
    && mkdir -p /out/licenses \
    && cp ffmpeg.exe /out/ffmpeg.exe \
    && cp COPYING.LGPLv2.1 /out/licenses/FFmpeg-LGPL-2.1.txt \
    && cp "/build/opus-${OPUS_VERSION}/COPYING" /out/licenses/libopus-BSD.txt

FROM scratch AS artifact
COPY --from=build /out/ /
