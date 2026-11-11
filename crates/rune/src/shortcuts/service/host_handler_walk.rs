//! Handlers against the hosts that took their registrations, in every interleaving of two hosts
//! and one caller, each split where the code takes and lets go of a lock. An attach puts its
//! engine in place and then waits for routing. A close takes routing, then the engine, then
//! cleans up. A registration reserves its id and handler, is routed, then reaches an engine.
//! Three sides play each sequence: the code on dev, that code with the marked departure, and the
//! new code. All three route through the real machine, which this change leaves alone. The new code matches the marked reference at every event, and the reference
//! matches the old code up to the event where the departure first shows.

use super::*;
use std::{collections::{BTreeMap, BTreeSet}, sync::LazyLock, thread::ThreadId};

const A: usize = 0;
const B: usize = 1;
/// The thread of a caller that is neither host.
const OTHER: usize = 2;

static THREADS: LazyLock<[ThreadId; 3]> = LazyLock::new(|| [(); 3].map(|()| std::thread::spawn(|| std::thread::current().id()).join().unwrap()));

const DROPPED_LATER: &str = "a cleanup dropped a handler routed after its close took routing";
const KEPT_EARLIER: &str = "a cleanup kept a handler routed before its close took routing";
const LOST_LIVE: &str = "the attached host's engine holds a registration with no handler";
const LEAKED: &str = "a handler outlived both hosts";
const KEEPS_LATER: &str = "a cleanup kept what was routed after its close, or not routed yet";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Side { Dev, Reference, New }

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Host {
    Unborn,
    /// Its engine is in place; its attach waits for routing.
    Installed,
    Attached,
    /// Its close took routing as the `epoch`th close, when `latest` was the latest host. On
    /// Linux `taken` is the engine it took from the slot, once it has.
    Closing { epoch: usize, latest: Attempt, taken: Option<Option<usize>> },
    /// Its close let go of the engine too.
    Released { epoch: usize, latest: Attempt },
    Closed,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Idle,
    Reserved,
    /// Holding routing while it waits for this engine's lock.
    Parked(usize),
    /// Waiting for an owner thread to drain it.
    Queued,
    /// On its way into `engine`, holding its lock when `locked`.
    Routed { engine: usize, locked: bool },
}

#[derive(Clone, Default)]
struct EngineState { held: BTreeSet<u32>, released: bool }

struct World {
    side: Side,
    linux: bool,
    hosts: [Host; 2],
    engines: Vec<EngineState>,
    slot: Option<usize>,
    locals: [Option<usize>; 2],
    attachment: Attachment<u32>,
    latest: Seq,
    /// Each handler, with the host that took it on the new side.
    handlers: BTreeMap<u32, Option<Attempt>>,
    /// How many closes had taken routing when each registration was routed.
    routed_after: BTreeMap<u32, usize>,
    closes: usize,
    call: Phase,
    id: u32,
    result: Option<Result<(), ShortcutError>>,
    /// Another call inside this engine, holding its lock.
    busy: Option<usize>,
    fired: BTreeSet<u32>,
    broken: Vec<&'static str>,
    /// What the last cleanup kept, and what it would keep if it kept only what wasn't routed
    /// before its close took routing.
    kept: Option<(BTreeSet<u32>, BTreeSet<u32>)>,
}

impl Clone for World {
    fn clone(&self) -> Self {
        let attachment = match &self.attachment {
            Attachment::Detached => Attachment::Detached,
            Attachment::Attached { owner, wake, jobs } => Attachment::Attached { owner: *owner, wake: wake.clone(), jobs: jobs.clone() },
            Attachment::Closed => Attachment::Closed,
        };
        World {
            side: self.side, linux: self.linux, hosts: self.hosts, engines: self.engines.clone(), slot: self.slot, locals: self.locals,
            attachment, latest: self.latest, handlers: self.handlers.clone(),
            routed_after: self.routed_after.clone(), closes: self.closes, call: self.call, id: self.id, result: self.result.clone(),
            busy: self.busy, fired: self.fired.clone(), broken: self.broken.clone(), kept: self.kept.clone(),
        }
    }
}

/// Everything an app could tell apart, with each host reduced to its phase.
type View = (Phase, Option<Result<(), ShortcutError>>, [u8; 2], Option<usize>, Vec<u32>, Vec<(usize, Vec<u32>)>, Option<usize>, Vec<u32>, Vec<(u32, bool)>);

impl World {
    fn new(side: Side, linux: bool) -> Self {
        let mut world = World {
            side, linux, hosts: [Host::Unborn; 2], engines: vec![], slot: None, locals: [None; 2],
            attachment: Attachment::Detached, latest: Seq::default(), handlers: BTreeMap::new(), routed_after: BTreeMap::new(),
            closes: 0, call: Phase::Idle, id: 0, result: None, busy: None, fired: BTreeSet::new(), broken: vec![], kept: None,
        };
        world.install(A);
        world.attach(A);
        world
    }

    fn routing_held(&self) -> bool { matches!(self.call, Phase::Parked(_)) }

    fn locked(&self, engine: usize) -> bool { self.busy == Some(engine) || self.call == Phase::Routed { engine, locked: true } }

    fn owner(&self) -> Option<usize> {
        let Attachment::Attached { owner, .. } = &self.attachment else { return None };
        THREADS.iter().position(|thread| thread == owner)
    }

    fn jobs(&self) -> Vec<u32> {
        let Attachment::Attached { jobs, .. } = &self.attachment else { return vec![] };
        jobs.clone()
    }

    /// The engine the attached host's calls reach, while it is live.
    fn current_engine(&self) -> Option<usize> {
        let owner = self.owner()?;
        let engine = if self.linux { self.slot } else { self.locals[owner] }?;
        (!self.engines[engine].released).then_some(engine)
    }

    fn install(&mut self, host: usize) {
        self.engines.push(EngineState::default());
        let engine = self.engines.len() - 1;
        if self.linux { self.slot = Some(engine) } else { self.locals[host] = Some(engine) }
        self.hosts[host] = Host::Installed;
    }

    fn attach(&mut self, host: usize) {
        self.latest.next();
        step_attachment(&mut self.attachment, AttachEvent::Attach { owner: THREADS[host], wake: Arc::new(|| {}) });
        self.hosts[host] = Host::Attached;
    }

    fn close_routing(&mut self, host: usize) {
        self.closes += 1;
        let jobs = match step_attachment(&mut self.attachment, AttachEvent::Close).into_iter().next() {
            Some(AttachEffect::Release(jobs)) => jobs,
            _ => vec![],
        };
        // A queued caller is let go with NotRunning, and drops its handler.
        if self.call == Phase::Queued && jobs.contains(&self.id) { self.refuse(); }
        self.hosts[host] = Host::Closing { epoch: self.closes, latest: self.latest.current(), taken: None };
    }

    /// Lets go of the engine. On Linux the close takes the slot's engine, whichever it is, then
    /// waits for its lock to release it.
    fn release_engine(&mut self, host: usize) -> bool {
        let Host::Closing { epoch, latest, taken } = self.hosts[host] else { unreachable!() };
        if !self.linux {
            self.locals[host] = None;
        } else {
            let engine = match taken { Some(engine) => engine, None => self.slot.take() };
            if let Some(engine) = engine && self.locked(engine) {
                self.hosts[host] = Host::Closing { epoch, latest, taken: Some(Some(engine)) };
                return taken.is_none();
            }
            if let Some(engine) = engine { self.engines[engine].released = true; }
        }
        self.hosts[host] = Host::Released { epoch, latest };
        true
    }

    fn cleanup(&mut self, host: usize) {
        let Host::Released { epoch, latest } = self.hosts[host] else { unreachable!() };
        let before: BTreeSet<u32> = self.handlers.keys().copied().collect();
        let routed = self.routed_after.clone();
        let routed_before = |id: &u32| routed.get(id).is_some_and(|after| *after < epoch);
        match self.side {
            Side::Dev => self.handlers.clear(),
            // differs from dev: a cleanup keeps what was routed after its close took routing, and
            // what isn't routed yet. Dev dropped every handler.
            Side::Reference => self.handlers.retain(|id, _| !routed_before(id)),
            Side::New => self.handlers.retain(|_, host| host.is_none_or(|host| host > latest)),
        }
        let after: BTreeSet<u32> = self.handlers.keys().copied().collect();
        if before.difference(&after).any(|id| !routed_before(id)) { self.broken.push(DROPPED_LATER); }
        if after.iter().any(routed_before) { self.broken.push(KEPT_EARLIER); }
        let expected = before.iter().copied().filter(|id| !routed_before(id)).collect();
        self.kept = Some((after, expected));
        self.hosts[host] = Host::Closed;
    }

    fn reserve(&mut self) {
        self.id += 1;
        self.handlers.insert(self.id, None);
        self.call = Phase::Reserved;
        self.result = None;
    }

    fn refuse(&mut self) {
        self.handlers.remove(&self.id);
        self.call = Phase::Idle;
        self.result = Some(Err(ShortcutError::NotRunning));
    }

    /// Routing hands the call to the latest host.
    fn mark_routed(&mut self) {
        self.routed_after.insert(self.id, self.closes);
        if self.side == Side::New {
            let latest = self.latest.current();
            if let Some(host) = self.handlers.get_mut(&self.id) { *host = Some(latest); }
        }
    }

    fn route(&mut self, from: usize) {
        if self.linux {
            let engine = match step_attachment(&mut self.attachment, AttachEvent::Call { from: THREADS[OTHER], shared: true }).into_iter().next() {
                Some(AttachEffect::Run { .. }) => self.slot,
                _ => None,
            };
            let Some(engine) = engine else { return self.refuse() };
            self.mark_routed();
            self.call = if self.locked(engine) { Phase::Parked(engine) } else { Phase::Routed { engine, locked: true } };
            return;
        }
        let run = match step_attachment(&mut self.attachment, AttachEvent::Call { from: THREADS[from], shared: false }).into_iter().next() {
            Some(AttachEffect::Run { .. }) => from,
            Some(AttachEffect::Wait) => {
                step_attachment(&mut self.attachment, AttachEvent::Post(self.id));
                self.call = Phase::Queued;
                return;
            }
            _ => return self.refuse(),
        };
        self.mark_routed();
        match self.locals[run] {
            Some(engine) => self.call = Phase::Routed { engine, locked: false },
            None => self.refuse(),
        }
    }

    fn drain(&mut self, host: usize) {
        let jobs = match step_attachment(&mut self.attachment, AttachEvent::Drain).into_iter().next() {
            Some(AttachEffect::Jobs(jobs)) => jobs,
            _ => vec![],
        };
        if self.call != Phase::Queued || !jobs.contains(&self.id) { return; }
        self.mark_routed();
        match self.locals[host] {
            Some(engine) => self.call = Phase::Routed { engine, locked: false },
            None => self.refuse(),
        }
    }

    fn run(&mut self) {
        let Phase::Routed { engine, .. } = self.call else { unreachable!() };
        self.engines[engine].held.insert(self.id);
        self.call = Phase::Idle;
        self.result = Some(Ok(()));
    }

    /// An engine nothing holds any more is dropped, which releases it.
    fn settle(&mut self) {
        let mut reachable: Vec<usize> = self.slot.into_iter().chain(self.locals.into_iter().flatten()).chain(self.busy).collect();
        if let Phase::Parked(engine) | Phase::Routed { engine, .. } = self.call { reachable.push(engine); }
        for host in self.hosts { if let Host::Closing { taken: Some(Some(engine)), .. } = host { reachable.push(engine); } }
        for (index, engine) in self.engines.iter_mut().enumerate() {
            if !reachable.contains(&index) { engine.released = true; }
        }
    }

    fn observe(&mut self) {
        self.settle();
        for engine in &self.engines { if !engine.released { self.fired.extend(&engine.held); } }
        if let Some(engine) = self.current_engine() && self.engines[engine].held.iter().any(|id| !self.handlers.contains_key(id)) {
            self.broken.push(LOST_LIVE);
        }
        let ended = |host: Host| matches!(host, Host::Closed | Host::Unborn);
        if self.hosts[A] == Host::Closed && ended(self.hosts[B]) && self.call == Phase::Idle && !self.handlers.is_empty() { self.broken.push(LEAKED); }
    }

    /// Letters: 0 B installs its engine, 1 B's attach takes routing; 2, 3, 4 A's close takes
    /// routing, lets go of the engine, cleans up; 5, 6, 7 the same for B; 8 reserve an id and
    /// handler; 9 route it from another thread; 10 route it from the attached host's thread; 11
    /// it reaches its engine; 12, 13 A or B drains; 14, 15 another call enters an engine and
    /// leaves; 16 a parked call takes the engine. Returns whether the letter did anything.
    fn play(&mut self, letter: usize) -> bool {
        self.kept = None;
        let host = if (2..5).contains(&letter) || letter == 12 { A } else { B };
        let moved = match letter {
            0 if self.hosts[B] == Host::Unborn => { self.install(B); true }
            1 if self.hosts[B] == Host::Installed && !self.routing_held() => { self.attach(B); true }
            2 | 5 if self.hosts[host] == Host::Attached && !self.routing_held() => { self.close_routing(host); true }
            3 | 6 if matches!(self.hosts[host], Host::Closing { .. }) => self.release_engine(host),
            4 | 7 if matches!(self.hosts[host], Host::Released { .. }) => { self.cleanup(host); true }
            8 if self.call == Phase::Idle => { self.reserve(); true }
            9 if self.call == Phase::Reserved => { self.route(OTHER); true }
            10 if self.call == Phase::Reserved && !self.linux => { self.route(self.owner().unwrap_or(OTHER)); true }
            11 if matches!(self.call, Phase::Routed { .. }) => { self.run(); true }
            12 | 13 if !self.linux && self.hosts[host] == Host::Attached && !self.jobs().is_empty() => { self.drain(host); true }
            14 if self.linux && self.busy.is_none() && !self.routing_held() && self.owner().is_some() && self.slot.is_some_and(|engine| !self.locked(engine)) => {
                self.busy = self.slot;
                true
            }
            15 if self.busy.is_some() => { self.busy = None; true }
            16 => match self.call {
                Phase::Parked(engine) if !self.locked(engine) => { self.call = Phase::Routed { engine, locked: true }; true }
                _ => false,
            },
            _ => false,
        };
        if moved { self.observe(); }
        moved
    }

    fn view(&self) -> View {
        let phase = |host: Host| match host {
            Host::Unborn => 0, Host::Installed => 1, Host::Attached => 2,
            Host::Closing { taken: None, .. } => 3, Host::Closing { .. } => 4, Host::Released { .. } => 5, Host::Closed => 6,
        };
        let live = self.engines.iter().enumerate().filter(|(_, engine)| !engine.released).map(|(index, engine)| (index, engine.held.iter().copied().collect())).collect();
        let deliveries = self.fired.iter().map(|id| (*id, self.handlers.contains_key(id))).collect();
        (self.call, self.result.clone(), self.hosts.map(phase), self.owner(), self.jobs(), live, self.busy, self.handlers.keys().copied().collect(), deliveries)
    }
}

/// The view without the handlers, and what delivering each activation would do.
fn without_handlers(view: View) -> View {
    let (call, result, hosts, owner, jobs, live, busy, _, _) = view;
    (call, result, hosts, owner, jobs, live, busy, vec![], vec![])
}

struct Walk { found: BTreeSet<&'static str>, departures: usize }

fn walk(sides: &[World; 3], departed: bool, sequence: &mut Vec<usize>, depth: usize, out: &mut Walk) {
    for letter in 0..17 {
        let mut next = sides.clone();
        let moved = next.each_mut().map(|world| world.play(letter));
        assert!(moved.iter().all(|m| *m == moved[0]), "{sequence:?} + {letter}: sides disagree on what can happen");
        if !moved[0] { continue; }
        sequence.push(letter);
        let [dev, reference, new] = &next;
        let at = || format!("linux {}, {sequence:?}", new.linux);
        assert!(new.broken.is_empty(), "{:?}: {}", new.broken, at());
        assert!(reference.broken.is_empty(), "reference {:?}: {}", reference.broken, at());
        out.found.extend(dev.broken.iter().copied());
        assert_eq!(new.view(), reference.view(), "{}", at());
        let mut now_departed = departed;
        if !departed && dev.view() != reference.view() {
            // differs from dev: only a cleanup, keeping what was routed after its close took
            // routing or isn't routed yet, where dev kept nothing.
            assert_eq!(without_handlers(dev.view()), without_handlers(reference.view()), "{}", at());
            let (kept, expected) = reference.kept.clone().unwrap_or_else(|| panic!("the first difference isn't a cleanup: {}", at()));
            assert!(dev.kept.as_ref().is_some_and(|(kept, _)| kept.is_empty()), "{}", at());
            assert!(!kept.is_empty() && kept == expected, "{}: kept {kept:?}, expected {expected:?}", at());
            now_departed = true;
            out.departures += 1;
        }
        if sequence.len() < depth { walk(&next, now_departed, sequence, depth, out); }
        sequence.pop();
    }
}

#[test]
fn every_interleaving_keeps_handlers_with_the_hosts_that_took_them() {
    for linux in [true, false] {
        let sides = [Side::Dev, Side::Reference, Side::New].map(|side| World::new(side, linux));
        let mut out = Walk { found: BTreeSet::new(), departures: 0 };
        walk(&sides, false, &mut vec![], 8, &mut out);
        for invariant in [DROPPED_LATER, LOST_LIVE] {
            assert!(out.found.contains(invariant), "linux {linux}: the walk never saw the old code break this, so it proves nothing: {invariant}");
        }
        assert!(out.departures > 0, "linux {linux}: {KEEPS_LATER} never took effect");
    }
}

#[test]
fn an_attach_waits_while_a_parked_call_holds_routing() {
    let mut world = World::new(Side::New, true);
    // Another call is inside A's engine; a registration parks behind it, holding routing.
    for letter in [14, 8, 9] { assert!(world.play(letter)); }
    assert_eq!(world.call, Phase::Parked(0));
    assert!(world.play(0), "B's engine goes in the slot under the slot's own lock");
    assert!(!world.play(1), "B's attach can't take routing from the parked call");
    assert!(!world.play(2), "nor can A's close");
    assert_eq!((world.hosts[B], world.owner()), (Host::Installed, Some(A)));
    assert!(world.play(15) && world.play(16), "the call inside leaves and the parked one takes A's engine");
    assert!(world.play(1), "routing is free again");
    assert!(world.play(11));
    assert_eq!(world.handlers.get(&1), Some(&Some(Attempt(1))), "the parked call went to the host routing had when it was routed");
}
