fn main() {
    // Un errore di compilazione Slint deve fermare la build con il messaggio del compilatore.
    if let Err(e) = slint_build::compile("ui/settings.slint") {
        eprintln!("{e}");
        std::process::exit(1);
    }
    // Icona dell'eseguibile: risorsa .res generata da scripts/icone.py, passata al linker MSVC
    // (che la converte da sé: niente rc.exe).
    let res = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../assets/icon/cliprec.res");
    println!("cargo:rerun-if-changed={}", res.display());
    println!("cargo:rustc-link-arg-bins={}", res.display());
}
