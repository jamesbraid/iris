//! The events an engine posts for the shell, bounded, and the hook that tells
//! the shell some wait.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};

use parking_lot::Mutex;

use crate::engine::{Event, Mark, Rect, DAMAGE_ANY};

/// Distinct damage marks waiting before they degrade to "anything".
const DAMAGE_MAX: usize = 16;
/// Distinct notes and DMA-fault pages kept; the rest are dropped.
const SEEN_MAX: usize = 256;

#[derive(Default)]
struct Queue {
    events: Vec<Event>,
    irq_pending: bool,
    notes: HashSet<String>,
    faults: HashSet<(u64, bool)>,
}

pub struct Outbox {
    q: Mutex<Queue>,
    notify: Option<Box<dyn Fn() + Send + Sync>>,
    notified: AtomicBool,
}

impl Outbox {
    /// `notify` is called when work waits or an event is posted, from any
    /// thread and possibly with engine locks held, and must only schedule the
    /// shell's pump. Once per batch: the next call follows a `drain` or `rearm`.
    pub fn new(notify: Option<Box<dyn Fn() + Send + Sync>>) -> Outbox {
        Outbox { q: Mutex::new(Queue::default()), notify, notified: AtomicBool::new(false) }
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
        out.extend(q.events.drain(..n));
        q.irq_pending = q.events.iter().any(|e| matches!(e, Event::Irq));
        // Cleared under the lock, so a post after this drain wakes again.
        self.notified.store(!q.events.is_empty(), Ordering::Release);
    }

    pub fn pending(&self) -> bool {
        !self.q.lock().events.is_empty()
    }

    pub fn clear(&self) {
        *self.q.lock() = Queue::default();
        self.notified.store(false, Ordering::Release);
    }
}
