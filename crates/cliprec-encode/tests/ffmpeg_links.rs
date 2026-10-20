//! Verifica che le DLL di FFmpeg 9 si carichino e che l'encoder AAC nativo esista.

#[test]
fn ffmpeg_dlls_load() {
    ffmpeg_the_third::init().unwrap();
    assert!(ffmpeg_the_third::encoder::find_by_name("aac").is_some());
}
