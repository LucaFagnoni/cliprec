# Build FFmpeg ridotta (piano, non ancora eseguito)

Scopo: far stare la distribuzione sotto i 40 MB (SPEC.md, tabella prestazioni). La build BtbN
usata oggi (`cargo xtask ffmpeg`) ha tutti i codec e le librerie esterne: le sole cinque DLL che
ClipRec importa (avcodec, avformat, avutil, swresample; swscale non è importata) pesano circa 115 MB (avcodec 91 MB, avformat 23 MB). Una build mirata scende a
circa 6-11 MB (stima, da misurare).

## Cosa serve davvero

Dal codice: encoder `h264_nvenc`, `hevc_nvenc`, `av1_nvenc`, `h264_qsv`, `hevc_qsv`, `hevc_amf`
(più i corrispondenti H.264/AV1 QSV/AMF che l'app può scegliere), encoder `aac` e `flac`,
`pcm_s24le`/`pcm_s32le`, muxer `mp4`, `matroska` (MKV/MKA), `flac`, `wav`. Nessun decoder, nessun
filtro (la conversione BGRA→NV12 è su GPU), nessun programma. Le DLL richieste sono avcodec,
avformat, avutil, swresample (verificato con `dumpbin /dependents`; swscale, avfilter e
avdevice non servono).

## Opzioni di configure

```sh
./configure \
  --prefix=/ffbuild/prefix --arch=x86_64 --target-os=mingw32 \
  --cross-prefix=x86_64-w64-mingw32- \
  --enable-shared --disable-static --enable-version3 --disable-debug --disable-doc \
  --disable-programs --disable-network --disable-autodetect \
  --disable-everything \
  --disable-avfilter --disable-avdevice --disable-postproc \
  --enable-encoder=h264_nvenc,hevc_nvenc,av1_nvenc,h264_qsv,hevc_qsv,av1_qsv,h264_amf,hevc_amf,av1_amf,aac,flac,pcm_s24le,pcm_s32le \
  --enable-muxer=mp4,matroska,flac,wav \
  --enable-parser=h264,hevc,av1,aac,flac \
  --enable-bsf=h264_mp4toannexb,hevc_mp4toannexb,extract_extradata,aac_adtstoasc \
  --enable-protocol=file \
  --enable-ffnvcodec --enable-nvenc \
  --enable-libvpl \
  --enable-amf \
  --enable-d3d11va --enable-dxva2
```

Note:

- `--disable-autodetect` toglie zlib, iconv, ecc.; si riattivano solo quelle richieste a mano.
  `--enable-libvpl` (QSV) richiede le intestazioni e la libreria oneVPL; il runtime (`libvpl`
  dispatcher) si carica dalla DLL dell'driver Intel, non va distribuito.
- `--enable-amf` usa solo intestazioni (carica `amfrt64.dll` del driver AMD a runtime).
- `--enable-ffnvcodec` carica `nvcuda`/`nvEncodeAPI64` a runtime: nessuna DLL NVIDIA da distribuire.
- Niente `--enable-gpl`/`--enable-nonfree`: resta LGPL 3 (`--enable-version3`), come richiede la
  distribuzione (vedi `THIRD_PARTY_NOTICES.md`). Niente fdk-aac, x264, x265.
- hwaccel d3d11va: serve solo se un giorno si decodifica; per la sola codifica si può anche
  togliere (`--disable-d3d11va`) e risparmiare qualche KB.
- La sezione `[hwaccels]` e il fallback software (SVT-AV1) non sono inclusi: SPEC.md li prevede
  solo se manca un encoder hardware; aggiungerlo costa ~3 MB (`--enable-libsvtav1`).

## Come compilarla

**Opzione A, MSYS2 (più semplice per provare):**

1. Installare MSYS2, aprire «MSYS2 MinGW x64», `pacman -S base-devel mingw-w64-x86_64-toolchain
   mingw-w64-x86_64-nasm mingw-w64-x86_64-pkgconf mingw-w64-x86_64-libvpl git`.
2. `git clone --branch n9.0.2 https://git.ffmpeg.org/ffmpeg.git && cd ffmpeg`, installare
   `nv-codec-headers` e le intestazioni AMF (`AMF/include` da GPUOpen) in `$MINGW_PREFIX`.
3. Eseguire il `./configure` sopra (senza `--cross-prefix`), `make -j8 && make install`.
4. Convertire le `.lib` per MSVC: `lib /def:avcodec.def /machine:x64` per ogni DLL (FFmpeg le
   crea con `--enable-shared` quando c'è `lib.exe` nel PATH).

**Opzione B, cross-compilazione come BtbN:** clonare `github.com/BtbN/FFmpeg-Builds`,
copiare `variants/win64-lgpl-shared.sh` in una variante nuova, sostituire gli `FF_CONFIGURE`
con le opzioni sopra e togliere le dipendenze in `scripts.d/` non necessarie, poi
`./makeimage.sh win64 lgpl-ridotta && ./build.sh win64 lgpl-ridotta` (richiede Docker).

Il risultato (`bin/*.dll`, `lib/*.lib`, `include/`) va in `third_party/ffmpeg` al posto della
build BtbN; `cargo xtask dist` non cambia. Rieseguire i test end-to-end (clip MP4 multitraccia,
clip FLAC, `cargo xtask probe`/`sync`) con la nuova libreria prima di distribuirla.

## Dimensioni attese

| Voce | Oggi | Ridotta (stima) |
| --- | --- | --- |
| avcodec | 87 MB | 3-6 MB |
| avformat | 22 MB | 1.5-3 MB |
| avutil, swresample | 3.7 MB | 1.5-2 MB |
| exe ClipRec + Slint | 18,5 MB (release, misurato) | uguale |
| Totale cartella | 130 MB (misurato) | ~26-30 MB (stima) |

Per rispettare la LGPL va distribuita anche la riga di configure usata e il link al sorgente:
`cargo xtask dist` li scrive in `FFmpeg-BUILD.txt` leggendoli da `ffmpeg -version`, quindi
`third_party/ffmpeg/bin/ffmpeg.exe` deve restare presente nella build di distribuzione (anche se
non viene copiato nell'installer). Con `--disable-programs` non esiste: in quel caso scrivere il
`FFmpeg-BUILD.txt` a mano (opzioni sopra) e adattare `xtask/src/dist.rs`.
