use std::sync::{Arc, Mutex};
use std::thread;

type Listener = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

#[derive(Clone)]
struct HarnessEventBus {
    delivery: Arc<Mutex<()>>,
    watcher: Listener,
}

impl HarnessEventBus {
    fn emit_batch(&self, event: &str) {
        let _delivery = self.delivery.lock().unwrap();
        self.deliver(event);
    }

    fn emit(&self, event: &str) {
        self.emit_batch(event);
    }

    fn deliver(&self, event: &str) {
        if let Err(error) = (self.watcher)(event) {
            eprintln!("watcher failed: {error}; recursively emitting handler_error");
            self.emit("handler_error");
        }
    }
}

fn main() {
    eprintln!("test event_bus_continues_watcher_delivery_after_listener_failure_and_reports_it ...");
    let bus = HarnessEventBus {
        delivery: Arc::new(Mutex::new(())),
        watcher: Arc::new(|event| {
            if event == "run_start" { Err("watcher failed".into()) } else { Ok(()) }
        }),
    };
    let worker = thread::Builder::new().name("event_bus_conti".into()).spawn(move || {
        bus.emit("run_start");
    }).unwrap();
    worker.join().unwrap();
    eprintln!("unexpected completion");
}
