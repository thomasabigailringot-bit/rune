//! A close removes the handlers of the hosts it ends and leaves a later host's alone, through
//! the real `Guard` and real threads.

use super::*;
use crate::{App, AppHandle, AppSpec, el};

fn app(title: &'static str) -> App<u32> { App::new(AppSpec::new(title, (10.0, 10.0), || 0, |_: &u32, _| el())) }

fn chord(text: &str) -> Shortcut { text.parse().unwrap() }

/// Runs every queued update, and returns how far the handlers moved the count.
fn deliver(app: &App<u32>) -> u32 {
    let mut state = 0;
    for f in app.shared.take().updates { f(&mut state, &mut Tx::default()); }
    state
}

/// A host attached on its own thread.
struct Host { close: mpsc::Sender<()>, entered: mpsc::Receiver<()>, resume: mpsc::Sender<()>, thread: std::thread::JoinHandle<()>, closing: bool }

impl Host {
    /// Attaches on a new thread and runs `then` there. Its close, once asked for, stops before
    /// its cleanup until `resume`.
    fn spawn(app: &App<u32>, then: impl FnOnce(&Arc<Shared<u32>>, &AppHandle<u32>) + Send + 'static) -> Self {
        let (shared, handle) = (app.shared.clone(), app.handle());
        let (attached, is_attached) = mpsc::channel();
        let (close, closes) = mpsc::channel::<()>();
        let (entered_tx, entered) = mpsc::channel();
        let (resume, resumed) = mpsc::channel::<()>();
        let thread = std::thread::spawn(move || {
            let mut guard = Guard::attach(&shared, Arc::new(|| {}), true, false);
            then(&shared, &handle);
            attached.send(()).unwrap();
            closes.recv().unwrap();
            let cleanup = guard.cleanup.take().unwrap();
            guard.cleanup = Some(Box::new(move |latest| {
                entered_tx.send(()).unwrap();
                resumed.recv().unwrap();
                cleanup(latest);
            }));
            drop(guard);
        });
        is_attached.recv().unwrap();
        Host { close, entered, resume, thread, closing: false }
    }

    /// Closes up to its cleanup, which waits for `finish`.
    fn close_until_cleanup(&mut self) {
        self.close.send(()).unwrap();
        self.entered.recv().unwrap();
        self.closing = true;
    }

    /// Closes, or finishes closing, all the way.
    fn finish(mut self) {
        if !self.closing { self.close_until_cleanup(); }
        self.resume.send(()).unwrap();
        self.thread.join().unwrap();
    }
}

#[test]
fn a_closing_host_leaves_a_later_hosts_handlers_alone() {
    let app = app("Successor");
    let handle = app.handle();
    let mut first = Host::spawn(&app, |_, handle| { handle.register_shortcut(chord("ctrl+alt+g"), |state, _| *state += 100).unwrap(); });
    first.close_until_cleanup();

    let guard = Guard::attach(&app.shared, Arc::new(|| {}), true, false);
    let id = handle.register_shortcut(chord("ctrl+alt+h"), |state, _| *state += 1).unwrap();
    first.finish();

    assert_eq!(app.shared.shortcuts.handlers.lock().unwrap().keys().copied().collect::<Vec<_>>(), [id], "only the closed host's handler went");
    app.shared.shortcuts.fire(&chord("ctrl+alt+h")).unwrap();
    assert_eq!(deliver(&app), 1, "the later host's shortcut still reaches its handler");
    drop(guard);
    assert!(app.shared.shortcuts.handlers.lock().unwrap().is_empty());
}

#[test]
fn a_registration_routed_after_a_close_keeps_its_handler() {
    let app = app("Late");
    let mut first = Host::spawn(&app, |_, _| {});
    // The id and handler exist before the close; the registration reaches a host only after it.
    let id = app.shared.shortcuts.reserve(Arc::new(|state: &mut u32, _: &mut Tx| *state += 1)).unwrap();
    first.close_until_cleanup();

    let guard = Guard::attach(&app.shared, Arc::new(|| {}), true, false);
    assert_eq!(app.shared.shortcuts.bind(id, chord("ctrl+alt+l")), Ok(id));
    first.finish();

    app.shared.shortcuts.fire(&chord("ctrl+alt+l")).unwrap();
    assert_eq!(deliver(&app), 1, "the host that took the registration keeps its handler");
    drop(guard);
}

#[test]
fn a_registration_routed_before_a_close_loses_its_handler() {
    let app = app("Early");
    let first = Host::spawn(&app, |shared, handle| {
        handle.register_shortcut(chord("ctrl+alt+e"), |state, _| *state += 1).unwrap();
        shared.shortcuts.fire(&chord("ctrl+alt+e")).unwrap();
    });
    first.finish();
    assert!(app.shared.shortcuts.handlers.lock().unwrap().is_empty());
    assert_eq!(deliver(&app), 0, "an activation queued before the close runs nothing after it");
}

#[test]
fn closing_a_later_host_also_ends_the_handlers_of_the_host_it_replaced() {
    let app = app("Replaced");
    let first = Host::spawn(&app, |shared, handle| {
        handle.register_shortcut(chord("ctrl+alt+r"), |state, _| *state += 1).unwrap();
        shared.shortcuts.fire(&chord("ctrl+alt+r")).unwrap();
    });
    drop(Guard::attach(&app.shared, Arc::new(|| {}), true, false));
    assert_eq!(deliver(&app), 0, "the service is closed, so the replaced host's activation runs nothing");
    first.finish();
}

#[test]
fn an_unrouted_registration_outlives_a_close_and_goes_when_refused() {
    let app = app("Unrouted");
    let first = Host::spawn(&app, |_, _| {});
    let id = app.shared.shortcuts.reserve(Arc::new(|state: &mut u32, _: &mut Tx| *state += 1)).unwrap();
    first.finish();
    assert!(app.shared.shortcuts.handlers.lock().unwrap().contains_key(&id), "no host had it, so no close takes it");
    assert_eq!(app.shared.shortcuts.bind(id, chord("ctrl+alt+u")), Err(ShortcutError::NotRunning));
    assert!(app.shared.shortcuts.handlers.lock().unwrap().is_empty(), "refused, it goes with its registration");
}
