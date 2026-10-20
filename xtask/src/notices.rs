//! `cargo xtask notices`: rigenera `THIRD_PARTY_NOTICES.md` dalle dipendenze reali di
//! `cliprec-app` e `cliprec-settings` (solo dipendenze normali, non di build né di test).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use serde_json::Value;

const ROOTS: [&str; 2] = ["cliprec-app", "cliprec-settings"];

const HEADER: &str = "# Avvisi di terze parti

ClipRec è distribuito insieme ai componenti elencati qui sotto. Questo file è generato da
`cargo xtask notices`: non modificarlo a mano.

## FFmpeg

ClipRec usa le librerie FFmpeg (libavcodec, libavformat, libavutil, libswresample)
come DLL separate, in una build condivisa configurata con `--enable-version3` e senza
`--enable-gpl` né `--enable-nonfree`: licenza **LGPL 2.1 o successiva / LGPL 3**. Il testo della
licenza è in `FFmpeg-LICENSE.txt`, la riga di configure della build usata in `FFmpeg-BUILD.txt`.
Le DLL si possono sostituire con una build propria compatibile (stesse versioni maggiori).
Sorgente: <https://ffmpeg.org/download.html> (tag n9.0.2) e <https://github.com/BtbN/FFmpeg-Builds>.

## Slint

L'interfaccia delle impostazioni usa Slint con la licenza **royalty-free 2.0**, che richiede
l'attribuzione: la pagina Informazioni mostra il widget `AboutSlint`.
<https://slint.dev/terms-and-conditions>

## Pacchetti Rust

| Pacchetto | Versione | Licenza |
| --- | --- | --- |
";

pub fn run(root: &Path) -> Result<()> {
    let out = Command::new("cargo")
        .args(["metadata", "--format-version", "1", "--filter-platform", "x86_64-pc-windows-msvc"])
        .current_dir(root)
        .output()
        .context("avvio di cargo metadata")?;
    if !out.status.success() {
        bail!("cargo metadata: {}", String::from_utf8_lossy(&out.stderr));
    }
    let meta: Value = serde_json::from_slice(&out.stdout)?;
    let text = format!("{HEADER}{}", table(&meta)?);
    std::fs::write(root.join("THIRD_PARTY_NOTICES.md"), text)?;
    println!("THIRD_PARTY_NOTICES.md aggiornato");
    Ok(())
}

/// Righe della tabella per le dipendenze esterne raggiunte dalle radici.
fn table(meta: &Value) -> Result<String> {
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
    let mut rows = BTreeMap::new();
    for id in seen {
        let p = packages[id];
        if p["source"].is_null() {
            continue; // crate del workspace
        }
        let name = p["name"].as_str().unwrap_or("?");
        let license = p["license"].as_str().unwrap_or("(vedi il pacchetto)");
        rows.insert((name.to_owned(), p["version"].as_str().unwrap_or("?").to_owned()), license.to_owned());
    }
    Ok(rows.iter().map(|((n, v), l)| format!("| {n} | {v} | {l} |\n")).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

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
