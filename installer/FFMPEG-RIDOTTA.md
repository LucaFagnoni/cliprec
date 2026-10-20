# Build FFmpeg ridotta

ClipRec linka e distribuisce una build FFmpeg 9 LGPL 3 con i soli componenti usati, compilata da
`cargo xtask ffmpeg-ridotta` in `third_party/ffmpeg-ridotta` (`FFMPEG_DIR` in `.cargo/config.toml`).
Serve a stare sotto i 40 MB di SPEC.md (tabella prestazioni). Motivi e versioni fissate:
docs/DECISIONS.md, voce 36.

## Cosa contiene

Dal codice di `cliprec-encode` e `cliprec-mux`:

- encoder `h264_nvenc`, `hevc_nvenc`, `av1_nvenc`, `h264_qsv`, `hevc_qsv`, `av1_qsv`, `h264_amf`,
  `hevc_amf`, `av1_amf`, `libsvtav1` (ripiego software), `aac`, `flac`, `pcm_s24le`, `libopus`;
- muxer `mp4`, `matroska` (MKV/MKA), `flac`, `wav`, `opus`; protocollo `file`;
- hwcontext D3D11VA e QSV (derivato da D3D11); swresample;
- bsf `aac_adtstoasc` e parser HEVC (richiesto dal link di `hevc/parse.o` in una build minima).

Niente decoder, filtri, avdevice, swscale, rete né programmi; niente `--enable-gpl` o
`--enable-nonfree`. Opzioni esatte: `scripts/ffmpeg-ridotta.sh`.

SVT-AV1, Opus e libvpl (dispatcher QSV) sono linkati staticamente, con il runtime di gcc
(`-static`): le quattro DLL (avcodec, avformat, avutil, swresample) importano solo DLL di Windows.
AMF e NVENC usano solo intestazioni e caricano il runtime del driver.

## Come si compila

`cargo xtask ffmpeg-ridotta` (circa 10 minuti, rete necessaria):

1. scarica MSYS2 base in `third_party/toolchain` e ne verifica lo SHA-256;
2. aggiorna MSYS2 e installa per URL le versioni fissate dei pacchetti (`xtask/src/ridotta.rs`);
3. scarica il sorgente di FFmpeg al commit fissato e ne verifica lo SHA-256;
4. lancia `scripts/ffmpeg-ridotta.sh` nella shell UCRT64: configure, make, install, librerie
   `.lib` per MSVC in `lib/` (create da `dlltool`), `LICENSE.txt` (LGPL 3) e `ffmpeg-version.txt`
   (versione e riga di configure, al posto di `ffmpeg -version`).

`cargo xtask dist` copia le quattro DLL e scrive `FFmpeg-BUILD.txt` (commit, URL e hash del
sorgente, toolchain e pacchetti, riga di configure), come chiede la LGPL.

La build completa di BtbN (`cargo xtask ffmpeg`, `third_party/ffmpeg`, `FFMPEG_TOOLS_DIR`) resta
solo per ffmpeg/ffprobe nei test e in `cargo xtask probe`/`sync`: decodifica e filtri che
ClipRec non usa.

## Dimensioni

| Voce | BtbN completa | Ridotta (misurato) |
| --- | --- | --- |
| avcodec | 86,9 MiB | 10,2 MiB |
| avformat | 21,7 MiB | 0,7 MiB |
| avutil | 2,9 MiB | 2,5 MiB |
| swresample | 0,7 MiB | 0,2 MiB |
| Totale DLL | 112,2 MiB | 13,5 MiB |

In avcodec il grosso è SVT-AV1 (encoder software con codice per AVX2/AVX-512); in avutil e avcodec
c'è due volte libstdc++ per libvpl. Totali di cartella, installer e zip: docs/PROGRESS.md.
