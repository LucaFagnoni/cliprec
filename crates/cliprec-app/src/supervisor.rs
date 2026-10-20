//! Supervisore: esegue un componente sotto `catch_unwind` e lo riavvia con backoff se va in panic.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct Backoff {
    pub initial: Duration,
    pub max: Duration,
    /// Se il componente ha girato almeno così a lungo prima del panic, il backoff riparte da `initial`.
    pub healthy_after: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self { initial: Duration::from_millis(250), max: Duration::from_secs(10), healthy_after: Duration::from_secs(30) }
    }
}

/// Avvia `f` su un thread chiamato `name`. Se `f` ritorna, il thread finisce; se va in panic
/// viene richiamata dopo un'attesa crescente. `f` deve poter ripartire da zero.
pub fn spawn_supervised<F>(name: &'static str, f: F) -> std::io::Result<JoinHandle<()>>
where
    F: FnMut() + Send + 'static,
{
    spawn_supervised_with(name, Backoff::default(), f)
}

pub fn spawn_supervised_with<F>(name: &'static str, backoff: Backoff, mut f: F) -> std::io::Result<JoinHandle<()>>
where
    F: FnMut() + Send + 'static,
{
    std::thread::Builder::new().name(name.into()).spawn(move || {
        let mut delay = backoff.initial;
        let mut restarts = 0u32;
        loop {
            let started = Instant::now();
            match catch_unwind(AssertUnwindSafe(&mut f)) {
                Ok(()) => {
                    tracing::info!(thread = name, "componente terminato");
                    return;
                }
                Err(payload) => {
                    let msg = payload
                        .downcast_ref::<&str>()
                        .map(|s| (*s).to_string())
                        .or_else(|| payload.downcast_ref::<String>().cloned())
                        .unwrap_or_else(|| "panic senza messaggio".into());
                    if started.elapsed() >= backoff.healthy_after {
                        delay = backoff.initial;
                    }
                    restarts += 1;
                    tracing::error!(thread = name, restarts, ?delay, "panic: {msg}; riavvio");
                    std::thread::sleep(delay);
                    delay = (delay * 2).min(backoff.max);
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    #[test]
    fn restarts_after_panics_then_ends_normally() {
        let runs = Arc::new(AtomicU32::new(0));
        let r = runs.clone();
        let backoff = Backoff { initial: Duration::from_millis(1), max: Duration::from_millis(4), healthy_after: Duration::from_secs(60) };
        let h = spawn_supervised_with("test", backoff, move || {
            if r.fetch_add(1, Ordering::SeqCst) < 2 {
                panic!("boom");
            }
        })
        .unwrap();
        h.join().unwrap();
        assert_eq!(runs.load(Ordering::SeqCst), 3, "due panic e una corsa buona");
    }
}
