//! The device-loss callback of a GPU unit that is opened before its engine.

use std::sync::Arc;

use parking_lot::Mutex;

type Handler = Arc<dyn Fn(&str) + Send + Sync>;

#[derive(Default)]
struct Inner {
    handler: Option<Handler>,
    /// A loss that came before `attach`: delivered by it.
    early: Option<String>,
}

/// Where a GPU unit's failure goes. The unit takes `callback` when it is
/// opened, before the engine exists; the engine then `attach`es the handler
/// that posts `Event::GpuLost` and faults it. The text is the unit's own,
/// which names the adapter.
#[derive(Default)]
pub struct GpuLoss {
    inner: Mutex<Inner>,
}

impl GpuLoss {
    pub fn new() -> Arc<GpuLoss> {
        Arc::default()
    }

    /// The callback for `Gpu::new`. It logs, and tells the handler on whichever
    /// thread the device failed.
    pub fn callback(self: &Arc<Self>) -> Box<dyn Fn(&str) + Send + Sync> {
        let this = self.clone();
        Box::new(move |why| {
            log::error!("GPU failed: {why}");
            let handler = {
                let mut i = this.inner.lock();
                if i.handler.is_none() {
                    i.early.get_or_insert_with(|| why.to_string());
                }
                i.handler.clone()
            };
            if let Some(h) = handler {
                h(why);
            }
        })
    }

    /// Set the engine's handler, and call it for a loss that came first.
    pub fn attach(&self, handler: impl Fn(&str) + Send + Sync + 'static) {
        let handler: Handler = Arc::new(handler);
        let early = {
            let mut i = self.inner.lock();
            i.handler = Some(handler.clone());
            i.early.take()
        };
        if let Some(why) = early {
            handler(&why);
        }
    }
}
