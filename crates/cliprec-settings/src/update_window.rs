//! Finestra di aggiornamento (`cliprec-settings --aggiornamento`): mostra la versione nuova e le
//! sue note, e segue sulla named pipe il core, che scarica, installa e si riavvia.

use std::time::Duration;

use cliprec_core::ipc::{IpcRequest, IpcResponse, UpdateInfo, UpdatePhase};
use slint::ComponentHandle;

use crate::i18n::Lang;
use crate::{UpdateWindow, pipe};

/// Cosa mostra la finestra in un dato momento.
#[derive(Debug, Default, PartialEq)]
pub struct View {
    pub heading: String,
    pub subheading: String,
    pub notes: String,
    pub status: String,
    pub error: bool,
    pub busy: bool,
    pub can_install: bool,
    pub install_text: String,
    pub close_text: String,
}

/// Stato della finestra tra un controllo e l'altro.
#[derive(Debug, Default)]
pub struct Tracker {
    /// Versione che si sta installando: dopo il riavvio il core nuovo deve riportarla.
    pub target: Option<String>,
}

/// Vista per lo stato `info` del core (`None` = core non raggiungibile).
pub fn view(lang: Lang, info: Option<&UpdateInfo>, tracker: &mut Tracker) -> View {
    let t = |it: &str, en: &str| lang.pick(it, en).to_string();
    let mut v = View { close_text: t("Chiudi", "Close"), install_text: t("Aggiorna e riavvia", "Update and restart"), ..View::default() };
    let Some(info) = info else {
        // Durante il riavvio il core per qualche secondo non risponde.
        if tracker.target.is_some() {
            v.heading = t("Riavvio di ClipRec…", "Restarting ClipRec…");
            v.busy = true;
        } else {
            v.heading = t("Aggiornamenti", "Updates");
            v.status = t("ClipRec non è in esecuzione.", "ClipRec is not running.");
            v.error = true;
        }
        return v;
    };
    if let Some(target) = &tracker.target
        && info.current == *target
    {
        v.heading = format!("{} {target}", t("ClipRec è aggiornato alla versione", "ClipRec has been updated to version"));
        v.notes.clone_from(&info.notes);
        return v;
    }
    let current = format!("{} {}", t("In uso:", "Installed:"), info.current);
    match (&info.phase, &info.available) {
        (UpdatePhase::Checking, _) => {
            v.heading = t("Controllo degli aggiornamenti…", "Checking for updates…");
            v.subheading = current;
            v.busy = true;
        }
        (UpdatePhase::UpToDate, _) | (UpdatePhase::Idle, None) => {
            v.heading = t("ClipRec è aggiornato", "ClipRec is up to date");
            v.subheading = format!("{} {}", t("Versione", "Version"), info.current);
        }
        (UpdatePhase::Failed(e), available) => {
            v.heading = match available {
                Some(a) => format!("{} {a}", t("È disponibile ClipRec", "ClipRec available:")),
                None => t("Aggiornamenti", "Updates"),
            };
            v.subheading = current;
            v.notes.clone_from(&info.notes);
            v.status = format!("{} {e}", t("Non riuscito:", "Failed:"));
            v.error = true;
            v.can_install = available.is_some();
            v.install_text = t("Riprova", "Retry");
        }
        (phase, available) => {
            let available = available.clone().unwrap_or_default();
            v.heading = format!("{} {available}", t("È disponibile ClipRec", "ClipRec available:"));
            v.subheading = current;
            v.notes.clone_from(&info.notes);
            v.can_install = true;
            v.close_text = t("Più tardi", "Later");
            let busy = match phase {
                UpdatePhase::Downloading => Some(t("Download dell'aggiornamento…", "Downloading the update…")),
                UpdatePhase::Installing => Some(t("Installazione…", "Installing…")),
                UpdatePhase::Restarting => Some(t("Riavvio di ClipRec…", "Restarting ClipRec…")),
                _ => None,
            };
            if let Some(s) = busy {
                v.status = s;
                v.busy = true;
                tracker.target = Some(available);
            }
        }
    }
    v
}

fn show(ui: &UpdateWindow, v: View, lang: Lang) {
    ui.set_heading(v.heading.into());
    ui.set_subheading(v.subheading.into());
    ui.set_notes_title(lang.pick("Novità", "What's new").into());
    ui.set_notes(v.notes.into());
    ui.set_status(v.status.into());
    ui.set_status_error(v.error);
    ui.set_busy(v.busy);
    ui.set_can_install(v.can_install);
    ui.set_install_text(v.install_text.into());
    ui.set_close_text(v.close_text.into());
}

/// Apre la finestra e la aggiorna due volte al secondo fino alla chiusura.
pub fn run(lang: Lang) -> anyhow::Result<()> {
    let ui = UpdateWindow::new()?;
    show(&ui, view(lang, None, &mut Tracker::default()), lang);
    ui.on_dismiss(|| {
        let _ = slint::quit_event_loop();
    });
    ui.on_install(|| {
        // L'esito si vede nello stato del core, letto due volte al secondo.
        std::thread::spawn(|| {
            let _ = pipe::request(&IpcRequest::InstallUpdate);
        });
    });
    let weak = ui.as_weak();
    std::thread::spawn(move || {
        let mut tracker = Tracker::default();
        loop {
            let info = match pipe::request(&IpcRequest::UpdateStatus) {
                Ok(IpcResponse::Update(i)) => Some(i),
                _ => None,
            };
            let v = view(lang, info.as_ref(), &mut tracker);
            if weak.upgrade_in_event_loop(move |ui| show(&ui, v, lang)).is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    });
    ui.run()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(phase: UpdatePhase, available: Option<&str>) -> UpdateInfo {
        UpdateInfo { current: "0.1.0".into(), available: available.map(Into::into), notes: "Novità".into(), phase }
    }

    #[test]
    fn available_update_can_be_installed_and_tracked_through_the_restart() {
        let mut tr = Tracker::default();
        let v = view(Lang::It, Some(&info(UpdatePhase::Idle, Some("0.2.0"))), &mut tr);
        assert!(v.can_install && !v.busy && v.heading.contains("0.2.0") && v.notes == "Novità");
        assert_eq!(v.close_text, "Più tardi");
        let v = view(Lang::It, Some(&info(UpdatePhase::Downloading, Some("0.2.0"))), &mut tr);
        assert!(v.busy && v.status.contains("Download"));
        // Il core si chiude per il riavvio: la finestra resta in attesa.
        let v = view(Lang::It, None, &mut tr);
        assert!(v.busy && !v.error);
        // Il core nuovo risponde con la versione installata.
        let mut new = info(UpdatePhase::Idle, None);
        new.current = "0.2.0".into();
        let v = view(Lang::It, Some(&new), &mut tr);
        assert!(v.heading.contains("aggiornato") && v.heading.contains("0.2.0") && !v.busy);
    }

    #[test]
    fn up_to_date_failure_and_offline_states() {
        let mut tr = Tracker::default();
        let v = view(Lang::It, Some(&info(UpdatePhase::UpToDate, None)), &mut tr);
        assert!(v.heading.contains("aggiornato") && !v.can_install);
        let v = view(Lang::It, Some(&info(UpdatePhase::Failed("rete assente".into()), Some("0.2.0"))), &mut tr);
        assert!(v.error && v.can_install && v.install_text == "Riprova" && v.status.contains("rete assente"));
        let v = view(Lang::It, None, &mut Tracker::default());
        assert!(v.error && !v.busy);
    }
}
