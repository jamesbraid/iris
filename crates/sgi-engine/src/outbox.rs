//! The events an engine posts for the shell, bounded, and the hook that tells
//! the shell some wait.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::engine::{Event, Interrupt, Mark, Rect, DAMAGE_ANY};

/// Distinct damage marks waiting before they degrade to "anything".
const DAMAGE_MAX: usize = 16;
/// Distinct notes and DMA-fault pages kept; the rest are dropped.
const SEEN_MAX: usize = 256;
/// Swap events waiting before the next is dropped and counted.
pub const SWAP_MAX: usize = 4096;

#[derive(Default)]
struct Queue {
    events: Vec<Event>,
    irq_pending: bool,
    notes: HashSet<String>,
    faults: HashSet<(u64, bool)>,
    swaps: usize,
}

pub struct Outbox {
    q: Mutex<Queue>,
    notify: Option<Box<dyn Fn() + Send + Sync>>,
    notified: AtomicBool,
    swaps_dropped: AtomicU64,
    /// Whether `q.events` is non-empty, written under the lock and readable without it.
    waiting: Arc<AtomicBool>,
}

impl Outbox {
    /// `notify` is called when work waits or an event is posted, from any
    /// thread and possibly with engine locks held, and must only schedule the
    /// shell's pump. Once per batch: the next call follows a `drain` or `rearm`.
    pub fn new(notify: Option<Box<dyn Fn() + Send + Sync>>) -> Outbox {
        Outbox { q: Mutex::new(Queue::default()), notify, notified: AtomicBool::new(false), swaps_dropped: AtomicU64::new(0), waiting: Arc::new(AtomicBool::new(false)) }
    }

    pub fn wake(&self) {
        if !self.notified.swap(true, Ordering::AcqRel) {
            if let Some(n) = &self.notify {
                n();
            }
        }
    }

    /// The pump ran: the next post or wake calls `notify` again.
    pub fn rearm(&self) {
        self.notified.store(false, Ordering::Release);
    }

    pub fn post(&self, e: Event) {
        {
            let mut q = self.q.lock();
            let fresh = match &e {
                Event::Irq => !std::mem::replace(&mut q.irq_pending, true),
                Event::Note(t) => q.notes.len() < SEEN_MAX && q.notes.insert(t.clone()),
                Event::BusFault { addr, write } => q.faults.len() < SEEN_MAX && q.faults.insert((addr >> 12, *write)),
                Event::Interrupt(i) => {
                    let line = |x: &Event| matches!(x, Event::Interrupt(o) if (o.sender, o.widget, o.addr, o.vector) == (i.sender, i.widget, i.addr, i.vector));
                    match q.events.iter().position(line) {
                        Some(at) if q.events[at] == e => false,
                        Some(at) => {
                            q.events.remove(at);
                            true
                        }
                        None => true,
                    }
                }
                Event::Swap { .. } => {
                    if q.swaps < SWAP_MAX {
                        q.swaps += 1;
                        true
                    } else {
                        self.swaps_dropped.fetch_add(1, Ordering::Relaxed);
                        false
                    }
                }
                Event::Credits { source, retired } => {
                    let queued = q.events.iter_mut().find_map(|x| match x {
                        Event::Credits { source: s, retired: r } if s == source => Some(r),
                        _ => None,
                    });
                    match queued {
                        Some(r) => {
                            *r = r.saturating_add(*retired);
                            false
                        }
                        None => *retired != 0,
                    }
                }
                _ => true,
            };
            if !fresh {
                return;
            }
            q.events.push(e);
            self.waiting.store(true, Ordering::Release);
        }
        self.wake();
    }

    /// Damage, joined per store and planes with what waits.
    pub fn damage(&self, marks: &[Mark]) {
        if marks.is_empty() {
            return;
        }
        {
            let mut q = self.q.lock();
            for m in marks {
                let same = |e: &Event| matches!(e, Event::Damage(d) if d.store == m.store && d.planes == m.planes);
                if let Some(Event::Damage(d)) = q.events.iter_mut().find(|e| same(e)) {
                    d.rect = d.rect.union(&m.rect);
                } else if q.events.iter().filter(|e| matches!(e, Event::Damage(_))).count() >= DAMAGE_MAX {
                    let any = q.events.iter().filter_map(|e| if let Event::Damage(d) = e { Some(d.rect) } else { None }).fold(m.rect, |a, r| a.union(&r));
                    q.events.retain(|e| !matches!(e, Event::Damage(_)));
                    q.events.push(Event::Damage(Mark { store: DAMAGE_ANY, planes: !0, rect: any }));
                } else {
                    q.events.push(Event::Damage(*m));
                }
            }
            self.waiting.store(true, Ordering::Release);
        }
        self.wake();
    }

    /// Everything on screen may have changed.
    pub fn damage_all(&self) {
        self.damage(&[Mark { store: DAMAGE_ANY, planes: !0, rect: Rect::new(0, 0, 4095, 4095) }]);
    }

    /// Move up to `max` events out, oldest first.
    pub fn drain(&self, out: &mut Vec<Event>, max: usize) {
        let mut q = self.q.lock();
        let n = q.events.len().min(max);
        let before = out.len();
        out.extend(q.events.drain(..n));
        q.swaps -= out[before..].iter().filter(|e| matches!(e, Event::Swap { .. })).count();
        q.irq_pending = q.events.iter().any(|e| matches!(e, Event::Irq));
        // Cleared under the lock, so a post after this drain wakes again.
        self.notified.store(!q.events.is_empty(), Ordering::Release);
        self.waiting.store(!q.events.is_empty(), Ordering::Release);
    }

    /// Whether events wait, as `pending` answers it, but readable from any thread without the queue lock.
    pub fn waiting(&self) -> Arc<AtomicBool> {
        self.waiting.clone()
    }

    /// Swap events dropped for want of room since the outbox was made.
    pub fn swaps_dropped(&self) -> u64 {
        self.swaps_dropped.load(Ordering::Relaxed)
    }

    pub fn pending(&self) -> bool {
        !self.q.lock().events.is_empty()
    }

    pub fn clear(&self) {
        let mut q = self.q.lock();
        *q = Queue::default();
        self.notified.store(false, Ordering::Release);
        self.waiting.store(false, Ordering::Release);
    }
}

/// Two packets whose merge keys differ but that reach one receiver bit, if any.
/// The merge rule is exact only when keys and receiver bits correspond one to
/// one: otherwise moving a packet to the back reorders two writes to a bit.
pub fn aliasing_lines<'a>(packets: impl IntoIterator<Item = &'a Interrupt>, bit: impl Fn(&Interrupt) -> u64) -> Option<(Interrupt, Interrupt)> {
    let mut seen: Vec<(u64, Interrupt)> = Vec::new();
    for p in packets {
        let b = bit(p);
        match seen.iter().find(|(sb, _)| *sb == b) {
            Some((_, q)) if (q.sender, q.widget, q.addr, q.vector) != (p.sender, p.widget, p.addr, p.vector) => return Some((*q, *p)),
            Some(_) => {}
            None => seen.push((b, *p)),
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Swap events are never merged, and past `SWAP_MAX` waiting the rest are dropped and counted; a drain makes room.
    #[test]
    fn swap_events_are_bounded_and_the_overflow_is_counted() {
        let o = Outbox::new(None);
        for i in 1..=(SWAP_MAX as u64 + 10) {
            o.post(Event::Swap { count: i, time_ns: i });
        }
        assert_eq!(o.swaps_dropped(), 10);
        let mut v = Vec::new();
        o.drain(&mut v, 100);
        assert_eq!(v.len(), 100);
        assert_eq!(v[99], Event::Swap { count: 100, time_ns: 100 });
        o.post(Event::Swap { count: 9999, time_ns: 0 });
        assert_eq!(o.swaps_dropped(), 10, "room again after a drain");
        o.drain(&mut v, usize::MAX);
        assert_eq!(v.iter().filter(|e| matches!(e, Event::Swap { .. })).count(), SWAP_MAX + 1);
    }
}
