//! Copia le DLL di FFmpeg accanto a eseguibili, test ed esempi, così girano senza toccare PATH.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-env-changed=FFMPEG_DIR");
    let Some(ffmpeg) = std::env::var_os("FFMPEG_DIR").map(PathBuf::from) else { return };
    let Some(out) = std::env::var_os("OUT_DIR").map(PathBuf::from) else { return };
    // OUT_DIR = target/<profilo>/build/<pacchetto>/out
    let Some(profile_dir) = out.ancestors().nth(3) else { return };
    let Ok(entries) = std::fs::read_dir(ffmpeg.join("bin")) else { return };
    let dlls: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("dll")))
        .collect();
    for dir in [profile_dir.to_path_buf(), profile_dir.join("deps"), profile_dir.join("examples")] {
        let _ = std::fs::create_dir_all(&dir);
        for dll in &dlls {
            if let Some(name) = dll.file_name() {
                let dest = dir.join(name);
                if !dest.exists() {
                    let _ = std::fs::copy(dll, dest);
                }
            }
        }
    }
}
