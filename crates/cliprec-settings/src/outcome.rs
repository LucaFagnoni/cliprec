//! Presentazione dell'esito della «Prova» (logica pura, senza Slint).

use std::time::Duration;

/// Esito di una «Prova» come lo presenta la finestra.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TestOutcome {
    Waiting,
    Done { text: String, error: bool },
    TimedOut,
}

/// Dopo «Prova» lo stato porta `last_test` entro qualche secondo. `baseline` è il valore che c'era al
/// clic; un testo diverso è l'esito nuovo. Se il testo è identico (stesso esito di prima) lo si
/// accetta dopo `SAME_RESULT_AFTER`; senza alcun esito dopo `TIMEOUT` si segnala.
pub fn test_progress(baseline: Option<&str>, current: Option<&str>, elapsed: Duration) -> TestOutcome {
    if let Some(text) = current
        && (Some(text) != baseline || elapsed >= SAME_RESULT_AFTER)
    {
        let low = text.to_lowercase();
        let error = ["errore", "silenz", "nero", "fall", "error", "silent", "black", "fail"]
            .iter()
            .any(|w| low.contains(w) && !low.contains("non nero") && !low.contains("not black"));
        return TestOutcome::Done { text: text.to_string(), error };
    }
    if elapsed >= TIMEOUT { TestOutcome::TimedOut } else { TestOutcome::Waiting }
}

const SAME_RESULT_AFTER: Duration = Duration::from_secs(8);
const TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    const S: fn(u64) -> Duration = Duration::from_secs;

    #[test]
    fn test_result_is_presented_when_it_arrives() {
        assert_eq!(test_progress(None, None, S(2)), TestOutcome::Waiting);
        assert_eq!(test_progress(Some("vecchio"), Some("vecchio"), S(2)), TestOutcome::Waiting);
        assert_eq!(
            test_progress(Some("vecchio"), Some("OK: 2 tracce con audio, video non nero"), S(3)),
            TestOutcome::Done { text: "OK: 2 tracce con audio, video non nero".into(), error: false }
        );
        assert_eq!(
            test_progress(None, Some("Traccia 2 silenziosa"), S(1)),
            TestOutcome::Done { text: "Traccia 2 silenziosa".into(), error: true }
        );
        // Stesso testo di prima: dopo qualche secondo è l'esito di questa prova.
        assert!(matches!(test_progress(Some("OK"), Some("OK"), S(9)), TestOutcome::Done { error: false, .. }));
        assert_eq!(test_progress(None, None, S(31)), TestOutcome::TimedOut);
    }
}
