fn main() {
    // Un errore di compilazione Slint deve fermare la build con il messaggio del compilatore.
    if let Err(e) = slint_build::compile("ui/settings.slint") {
        eprintln!("{e}");
        std::process::exit(1);
    }
}
