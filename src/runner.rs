//! One background thread for slow jobs (FineTune URLs, audio tools, COM calls), so the plugin's loop
//! never waits on them. A job with a key replaces a still-waiting job with the same key — a fader sweep only
//! needs its last position; jobs without a key all run (every step counts).

use std::collections::HashMap;
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

type Job = (Option<String>, Box<dyn FnOnce() + Send>);

pub fn submit(key: Option<String>, job: impl FnOnce() + Send + 'static) {
    static QUEUE: OnceLock<Mutex<Sender<Job>>> = OnceLock::new();
    let queue = QUEUE.get_or_init(|| {
        let (tx, rx) = mpsc::channel::<Job>();
        std::thread::spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut batch = vec![first];
                batch.extend(rx.try_iter());
                let mut latest: HashMap<String, usize> = HashMap::new();
                for (i, (key, _)) in batch.iter().enumerate() {
                    if let Some(k) = key {
                        latest.insert(k.clone(), i);
                    }
                }
                for (i, (key, job)) in batch.into_iter().enumerate() {
                    if key.as_ref().is_none_or(|k| latest[k] == i) {
                        // one failing job (an audio backend, a COM call) mustn't take the queue down with it
                        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
                            eprintln!("a background job failed");
                        }
                    }
                }
                // ponytail: fixed 30 ms between batches; at most ~30 fader updates a second reach the system
                std::thread::sleep(Duration::from_millis(30));
            }
        });
        Mutex::new(tx)
    });
    let _ = queue.lock().unwrap().send((key, Box::new(job)));
}
