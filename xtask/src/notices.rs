//! `cargo xtask notices`: rigenera `THIRD_PARTY_NOTICES.md` dalle dipendenze reali di
//! `cliprec-app` e `cliprec-settings` (solo dipendenze normali, non di build né di test) e dalla
//! riga di configure di FFmpeg. Fornisce anche i file di licenza che `cargo xtask dist` copia.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

const ROOTS: [&str; 2] = ["cliprec-app", "cliprec-settings"];

const HEADER: &str = "# Avvisi di terze parti

ClipRec è distribuito insieme ai componenti elencati qui sotto. Questo file è generato da
`cargo xtask notices`: non modificarlo a mano. I testi delle licenze sono nella cartella
`licenses/` della distribuzione (GPL 3, LGPL 3, LGPL 2.1, Apache 2.0, MIT, BSD, ISC, e i file di
licenza di ogni crate Rust in `licenses/crates/`).

## FFmpeg

ClipRec usa le librerie FFmpeg (libavcodec, libavformat, libavutil, libswresample)
come DLL separate, in una build condivisa configurata con `--enable-version3` e senza
`--enable-gpl` né `--enable-nonfree`: licenza **LGPL 3** (o successiva). Il testo è in
`FFmpeg-LICENSE.txt` e `licenses/LGPL-3.0.txt`; la LGPL 3 rinvia alla **GPL 3**, il cui testo è in
`licenses/GPL-3.0.txt`. La versione esatta, il commit, la riga di configure, l'URL del sorgente e
quello degli script di build sono in `FFmpeg-BUILD.txt`. Le DLL si possono sostituire con una build
propria compatibile (stesse versioni maggiori delle librerie).

## Slint

L'interfaccia delle impostazioni usa Slint con la licenza **royalty-free 2.0**, che richiede
l'attribuzione: la pagina Informazioni mostra il widget `AboutSlint`.
<https://slint.dev/terms-and-conditions>

";

const RUST_HEADER: &str = "
## Pacchetti Rust

| Pacchetto | Versione | Licenza |
| --- | --- | --- |
";

/// Librerie che le DLL di FFmpeg possono contenere, per nome dell'opzione `--enable-<nome>`:
/// (opzione, libreria, licenza, titolare). Tratte dai progetti originali; verificare sui sorgenti.
const LIBS: &[(&str, &str, &str, &str)] = &[
    ("zlib", "zlib", "zlib", "Jean-loup Gailly e Mark Adler"),
    ("iconv", "iconv", "dominio pubblico (win-iconv) o LGPL-2.1+ (GNU libiconv)", "Yukihiro Nakadaira / Free Software Foundation"),
    ("libxml2", "libxml2", "MIT", "Daniel Veillard e collaboratori"),
    ("libvmaf", "libvmaf", "BSD-2-Clause-Patent", "Netflix, Inc."),
    ("fontconfig", "Fontconfig", "MIT (stile HPND)", "Keith Packard e collaboratori"),
    ("libharfbuzz", "HarfBuzz", "MIT", "Behdad Esfahbod e collaboratori"),
    ("libfreetype", "FreeType", "FreeType License (FTL) o GPL-2.0", "The FreeType Project"),
    ("libfribidi", "FriBidi", "LGPL-2.1+", "Behdad Esfahbod, Dov Grobgeld e collaboratori"),
    ("lcms2", "Little CMS 2", "MIT", "Marti Maria Saguer"),
    ("vulkan", "Vulkan (intestazioni e loader)", "Apache-2.0", "The Khronos Group"),
    ("libdav1d", "dav1d", "BSD-2-Clause", "VideoLAN e gli autori di dav1d"),
    ("libvorbis", "libvorbis", "BSD-3-Clause", "Xiph.Org Foundation"),
    ("librav1e", "rav1e", "BSD-2-Clause-Patent", "Xiph.Org e i collaboratori di rav1e"),
    ("librsvg", "librsvg", "LGPL-2.1+", "Progetto GNOME e collaboratori"),
    ("gmp", "GMP", "LGPL-3.0+ o GPL-2.0+", "Free Software Foundation"),
    ("lzma", "XZ Utils (liblzma)", "0BSD / dominio pubblico", "Lasse Collin"),
    ("liblcevc-dec", "LCEVC decoder", "BSD-3-Clause-Clear", "V-Nova International Ltd."),
    ("opencl", "OpenCL (intestazioni e ICD loader)", "Apache-2.0", "The Khronos Group"),
    ("amf", "AMD AMF (intestazioni)", "MIT", "Advanced Micro Devices, Inc."),
    ("libaom", "libaom", "BSD-2-Clause con licenza di brevetti AOM", "Alliance for Open Media"),
    ("chromaprint", "Chromaprint", "MIT (LGPL-2.1+ con altri backend FFT)", "Lukáš Lalinský"),
    ("ffnvcodec", "nv-codec-headers", "MIT", "NVIDIA Corporation e il progetto FFmpeg"),
    ("libgme", "Game Music Emu", "LGPL-2.1+", "Shay Green, Michael Pyne e collaboratori"),
    ("libkvazaar", "Kvazaar", "BSD-3-Clause", "Tampere University e collaboratori"),
    ("libaribb24", "aribb24", "LGPL-3.0", "Nogami Ryo, Kouji Kobayashi e collaboratori"),
    ("libaribcaption", "aribcaption", "MIT", "magicxqq (xqq)"),
    ("libass", "libass", "ISC", "Contributori di libass"),
    ("libbluray", "libbluray", "LGPL-2.1+", "VideoLAN e collaboratori"),
    ("libjxl", "libjxl", "BSD-3-Clause", "Google LLC e i JPEG XL Project Authors"),
    ("libmp3lame", "LAME", "LGPL-2.0+", "The LAME Project"),
    ("libopus", "Opus", "BSD-3-Clause", "Xiph.Org Foundation, Skype Limited, Broadcom e altri"),
    ("libplacebo", "libplacebo", "LGPL-2.1+", "Niklas Haas e collaboratori"),
    ("librist", "librist", "BSD-2-Clause", "VideoLAN e SipRadius LLC"),
    ("libssh", "libssh", "LGPL-2.1+", "Contributori di libssh"),
    ("libtheora", "libtheora", "BSD-3-Clause", "Xiph.Org Foundation"),
    ("libvpx", "libvpx", "BSD-3-Clause con licenza di brevetti", "Google LLC e collaboratori"),
    ("libwebp", "libwebp", "BSD-3-Clause", "Google LLC"),
    ("libzmq", "ZeroMQ", "LGPL-3.0+ con eccezione per il linking statico (MPL-2.0 nelle versioni recenti)", "Contributori di ZeroMQ"),
    ("lv2", "LV2", "ISC", "David Robillard e collaboratori"),
    ("libvpl", "oneVPL", "MIT", "Intel Corporation"),
    ("openal", "OpenAL Soft", "LGPL-2.0+", "Chris Robinson e collaboratori"),
    ("liboapv", "OpenAPV", "BSD-3-Clause", "Samsung Electronics e collaboratori"),
    ("libopencore-amrnb", "OpenCORE AMR", "Apache-2.0", "VisualOn e Android Open Source Project"),
    ("libopencore-amrwb", "OpenCORE AMR", "Apache-2.0", "VisualOn e Android Open Source Project"),
    ("libopenh264", "OpenH264", "BSD-2-Clause", "Cisco Systems, Inc."),
    ("libopenjpeg", "OpenJPEG", "BSD-2-Clause", "Università di Lovanio e collaboratori"),
    ("libopenmpt", "libopenmpt", "BSD-3-Clause", "OpenMPT Project Developers and Contributors"),
    ("schannel", "Schannel", "componente di Windows, nessuna licenza di terzi", "Microsoft Corporation"),
    ("sdl2", "SDL 2", "zlib", "Sam Lantinga e collaboratori"),
    ("libsnappy", "Snappy", "BSD-3-Clause", "Google Inc."),
    ("libsoxr", "SoX Resampler", "LGPL-2.1+", "Rob Sykes"),
    ("libsrt", "SRT", "MPL-2.0", "Haivision Systems Inc."),
    ("libsvtav1", "SVT-AV1", "BSD-3-Clause-Clear con licenza di brevetti AOM", "Alliance for Open Media e Intel Corporation"),
    ("libtwolame", "TwoLAME", "LGPL-2.1+", "Michael Somos, Mark Taylor e collaboratori"),
    ("libuavs3d", "uavs3d", "Apache-2.0 / BSD (verificare sul sorgente)", "Peking University Shenzhen Graduate School e collaboratori"),
    ("vaapi", "VA-API", "MIT", "Intel Corporation e collaboratori"),
    ("vapoursynth", "VapourSynth", "LGPL-2.1+", "Fredrik Mellbin e collaboratori"),
    ("libvvenc", "VVenC", "BSD-3-Clause-Clear con licenza di brevetti", "Fraunhofer HHI"),
    ("libzimg", "zimg", "WTFPL", "Victor Prokhorov (sekrit-twc)"),
    ("libzvbi", "zvbi", "LGPL-2.1+ e BSD (alcune parti GPL-2, verificare sul sorgente)", "Michael H. Schimek, Iñaki García Etxebarria e collaboratori"),
    ("pthreads", "winpthreads (mingw-w64)", "MIT / ZPL", "Progetto mingw-w64"),
];

pub fn run(root: &Path) -> Result<()> {
    let meta = load_meta(root)?;
    let ffmpeg = std::env::var_os("FFMPEG_DIR").map(PathBuf::from).unwrap_or_else(|| root.join("third_party").join("ffmpeg"));
    let version = Command::new(ffmpeg.join("bin").join("ffmpeg.exe")).arg("-version").output().context("ffmpeg -version (cargo xtask ffmpeg?)")?;
    let text = format!(
        "{HEADER}{}{RUST_HEADER}{}",
        static_libs_section(&String::from_utf8_lossy(&version.stdout)),
        table(&meta)?
    );
    std::fs::write(root.join("THIRD_PARTY_NOTICES.md"), text)?;
    println!("THIRD_PARTY_NOTICES.md aggiornato");
    Ok(())
}

pub fn load_meta(root: &Path) -> Result<Value> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--filter-platform", "x86_64-pc-windows-msvc"])
        .current_dir(root)
        .output()
        .context("avvio di cargo metadata")?;
    if !out.status.success() {
        bail!("cargo metadata: {}", String::from_utf8_lossy(&out.stderr));
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// Sezione delle librerie linkate staticamente nelle DLL di FFmpeg, dalla riga di configure.
pub fn static_libs_section(version_output: &str) -> String {
    let configure = version_output.lines().find(|l| l.starts_with("configuration:")).unwrap_or("");
    let enabled: BTreeSet<&str> = configure.split_whitespace().filter_map(|t| t.strip_prefix("--enable-")).collect();
    let mut s = String::from(
        "## Librerie incluse nelle DLL di FFmpeg\n\n\
         La build FFmpeg usata contiene, linkate staticamente nelle DLL, le librerie seguenti\n\
         (ricavate dalla riga di configure in `FFmpeg-BUILD.txt`). Licenze e titolari sono quelli dei\n\
         progetti originali: vanno verificati sui sorgenti della build indicata.\n\n\
         | Libreria | Licenza | Titolare |\n| --- | --- | --- |\n",
    );
    let mut seen = BTreeSet::new();
    for name in enabled {
        match LIBS.iter().find(|l| l.0 == name) {
            Some((_, lib, lic, holder)) if seen.insert(*lib) => s.push_str(&format!("| {lib} | {lic} | {holder} |\n")),
            Some(_) => {}
            None if name.starts_with("lib") => s.push_str(&format!("| {name} | da verificare | da verificare |\n")),
            None => {}
        }
    }
    s
}

pub struct Crate {
    pub name: String,
    pub version: String,
    pub license: String,
    pub dir: PathBuf,
}

/// Dipendenze esterne (non del workspace) raggiunte dalle radici con dipendenze normali.
pub fn crates(meta: &Value) -> Result<Vec<Crate>> {
    let nodes: HashMap<&str, &Value> = meta["resolve"]["nodes"]
        .as_array()
        .context("resolve senza nodi")?
        .iter()
        .filter_map(|n| Some((n["id"].as_str()?, n)))
        .collect();
    let packages: HashMap<&str, &Value> =
        meta["packages"].as_array().context("senza pacchetti")?.iter().filter_map(|p| Some((p["id"].as_str()?, p))).collect();
    let mut todo: Vec<&str> =
        packages.iter().filter(|(_, p)| p["name"].as_str().is_some_and(|n| ROOTS.contains(&n))).map(|(id, _)| *id).collect();
    if todo.len() != ROOTS.len() {
        bail!("radici non trovate in cargo metadata");
    }
    let mut seen: HashSet<&str> = todo.iter().copied().collect();
    while let Some(id) = todo.pop() {
        let Some(node) = nodes.get(id) else { continue };
        for dep in node["deps"].as_array().into_iter().flatten() {
            let normal = dep["dep_kinds"].as_array().is_some_and(|k| k.iter().any(|k| k["kind"].is_null()));
            if let Some(d) = dep["pkg"].as_str().filter(|_| normal)
                && seen.insert(d)
            {
                todo.push(d);
            }
        }
    }
    let mut v = Vec::new();
    for id in seen {
        let p = packages[id];
        if p["source"].is_null() {
            continue; // crate del workspace
        }
        v.push(Crate {
            name: p["name"].as_str().unwrap_or("?").to_owned(),
            version: p["version"].as_str().unwrap_or("?").to_owned(),
            license: p["license"].as_str().unwrap_or("").to_owned(),
            dir: Path::new(p["manifest_path"].as_str().unwrap_or("")).parent().map(Path::to_path_buf).unwrap_or_default(),
        });
    }
    v.sort_by(|a, b| (&a.name, &a.version).cmp(&(&b.name, &b.version)));
    Ok(v)
}

/// Righe della tabella per le dipendenze esterne.
fn table(meta: &Value) -> Result<String> {
    let rows: BTreeMap<_, _> = crates(meta)?
        .into_iter()
        .map(|c| ((c.name, c.version), if c.license.is_empty() { "(vedi il pacchetto)".to_owned() } else { c.license }))
        .collect();
    Ok(rows.iter().map(|((n, v), l)| format!("| {n} | {v} | {l} |\n")).collect())
}

/// File di licenza distribuiti col sorgente di un crate (LICENSE*, COPYING*, UNLICENSE*, NOTICE*
/// nella radice, più la cartella `LICENSES/` di Slint).
pub fn license_files(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    for (d, any) in [(dir.to_path_buf(), false), (dir.join("LICENSES"), true)] {
        for e in std::fs::read_dir(&d).into_iter().flatten().flatten() {
            let name = e.file_name().to_string_lossy().to_ascii_uppercase();
            let wanted = any || ["LICENSE", "LICENCE", "COPYING", "UNLICENSE", "NOTICE"].iter().any(|p| name.starts_with(p));
            if wanted && e.path().is_file() {
                v.push(e.path());
            }
        }
    }
    v.sort();
    v
}

/// Identificativi SPDX di un'espressione di licenza (separatori OR, AND, `/`, parentesi).
fn spdx_ids(expr: &str) -> Vec<String> {
    expr.replace(['(', ')', '/'], " ")
        .split_whitespace()
        .filter(|t| !matches!(*t, "OR" | "AND" | "WITH"))
        .map(str::to_owned)
        .collect()
}

/// Crate (nome-versione) senza alcun testo di licenza incluso: né un file di licenza nel sorgente
/// né un testo generico `installer/licenses/<SPDX>.txt` per uno degli identificativi.
pub fn uncovered(meta: &Value, root: &Path) -> Result<Vec<String>> {
    let generic = |id: &str| root.join("installer").join("licenses").join(format!("{id}.txt")).exists();
    Ok(crates(meta)?
        .into_iter()
        .filter(|c| license_files(&c.dir).is_empty() && !spdx_ids(&c.license).iter().any(|id| generic(id)))
        .map(|c| format!("{}-{} ({})", c.name, c.version, c.license))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn static_libs_section_covers_configure() {
        let cfg = include_str!("../../installer/ffmpeg-version.txt");
        let t = static_libs_section(cfg);
        for lib in ["dav1d", "libvpx", "libaom", "SVT-AV1", "OpenH264", "LAME", "Opus", "libjxl", "zvbi"] {
            assert!(t.contains(lib), "sezione senza {lib}:\n{t}");
        }
        assert!(!t.contains("da verificare"), "libreria senza licenza nota:\n{t}");
    }

    #[test]
    fn every_crate_license_is_covered() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let meta = load_meta(&root).unwrap();
        assert_eq!(uncovered(&meta, &root).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn spdx_ids_split() {
        assert_eq!(spdx_ids("(MIT OR Apache-2.0) AND Unicode-3.0"), ["MIT", "Apache-2.0", "Unicode-3.0"]);
        assert_eq!(spdx_ids("MIT/Apache-2.0"), ["MIT", "Apache-2.0"]);
    }

    #[test]
    fn table_follows_normal_deps_only() {
        let meta = serde_json::json!({
            "packages": [
                {"id": "a", "name": "cliprec-app", "source": null, "version": "0.1.0", "license": null},
                {"id": "b", "name": "cliprec-settings", "source": null, "version": "0.1.0", "license": null},
                {"id": "c", "name": "serde", "source": "registry", "version": "1.0.0", "license": "MIT OR Apache-2.0"},
                {"id": "d", "name": "cc", "source": "registry", "version": "1.0.0", "license": "MIT"}
            ],
            "resolve": {"nodes": [
                {"id": "a", "deps": [
                    {"pkg": "c", "dep_kinds": [{"kind": null}]},
                    {"pkg": "d", "dep_kinds": [{"kind": "build"}]}]},
                {"id": "b", "deps": []}
            ]}
        });
        assert_eq!(table(&meta).unwrap(), "| serde | 1.0.0 | MIT OR Apache-2.0 |\n");
    }
}
