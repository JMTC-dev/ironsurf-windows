//! Web Workers — real threads, real realms, messages over a queue.
//!
//! `new Worker(url)` and `new Worker(URL.createObjectURL(new Blob([…])))` are
//! the two shapes every deployed Anubis gate uses: the challenge runs inside a
//! Blob worker the page's own inline glue creates, and the classic
//! `worker/worker.js` form is what every other site's worker uses. A worker
//! here is a **real OS thread** with its **own `boa_engine::Context`** and its
//! own realm — two threads never share a realm (Boa realms are `Rc`/thread
//! machinery), so the isolation the spec describes falls out of the design
//! instead of being simulated.
//!
//! # Shape
//!
//! * [`WorkerHost`] lives in [`HostState`](crate::js::HostState) — one per
//!   navigation. It owns the worker registry, the `blob:` URL store and the
//!   inbound message queue. Cloning it is cheap (every handle is an `Arc`) and
//!   the document-realm natives [`register_main_bindings`] installs hold one.
//! * One thread per live worker ([`WorkerThread`]): it builds its realm
//!   (`boa_runtime` baseline + the `__ironsurfWorker*` natives +
//!   [`WORKER_PRELUDE`]), runs the worker script, then pumps its inbox —
//!   messages, then due timers. The tab thread never blocks on it and it never
//!   touches the tab realm: the only shared state is the message queue, the
//!   wake pipe and the network cells (cookie jar, DevTools queue, User-Agent),
//!   all of them `Send + Sync`.
//! * Messages cross as **JSON text in both directions** (see the honest-limit
//!   note below). The tab drains its inbox in its poll loop
//!   ([`JsRuntime::deliver_worker_messages`](crate::js::JsRuntime::deliver_worker_messages))
//!   and the worker drains its own on its thread; both dispatch a `message`
//!   event with `onmessage` first, then `addEventListener` listeners in
//!   registration order.
//!
//! # The honest limit: structured clone is JSON here
//!
//! `postMessage` is specified with the **structured clone** algorithm, whose
//! observable difference from JSON is that it carries *live* values — `Map`,
//! `Set`, `Date`, `RegExp`, typed arrays and array buffers, `File`/`Blob`
//! handles, and above all `MessagePort`/`MessageChannel` endpoints, which are
//! transferable: a message can hand a live channel between two threads. None
//! of that survives a JSON hop: `{m: new Map([[1, 2]])}` would arrive as
//! `{m: {}}` and a port would arrive as `{}`.
//!
//! IronSurf clones through JSON and **says so in the surface it exposes**:
//!
//! * `worker.postMessage(data)` takes the data and an optional transfer list;
//!   the list is ignored because this engine has no live port to transfer, and
//!   the delivered event's `ports` is always `[]`.
//! * `MessageChannel`/`MessagePort`/`BroadcastChannel` are **not provided** in
//!   either realm: a page that needs a port gets a `ReferenceError` instead of
//!   an object that silently never delivers.
//! * Typed data crosses as JSON *text* — a worker that wants bytes posts
//!   base64/latin1 text (Anubis does exactly this with its hex/JSON payloads).
//!
//! What that costs in the wild: a page that talks to its worker over
//! `MessageChannel` (Comlink's transfer path, some worker pools, some
//! dev-server bridges) cannot connect. A worker that computes and answers —
//! every Anubis/hashcash/PoW captcha, syntax highlighter, PDF splitter and
//! virtualized-list precomputer in the wild — works, and finishes in seconds
//! instead of the minutes it took once the gate was open.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use boa_engine::{
    js_string, Context, JsNativeError, JsResult, JsString, JsValue, NativeFunction, Source,
};
use ironsurf_common::{debug, error, warn, NetEvent};
use url::Url;

use crate::js::{next_random_u64, SharedCookieJar};
use crate::netmod::{self, SharedUserAgent};

// ---------------------------------------------------------------------------
// Public surface

/// The tab's script-body backend, handed to the worker host on
/// [`WorkerHost::attach`] so the worker thread needs nothing from the tab but
/// the URL: `blob:` bodies need no fetch, every other worker script and
/// `importScripts` body goes through the tab's own `fetch_script_body`
/// (managed policy, cookie jar, redirects, DevTools queue, archive sink), so a
/// worker's script requests are indistinguishable from a `<script src>` in the
/// network log.
pub type WorkerFetch = Arc<dyn Fn(&Url) -> Result<String, String> + Send + Sync + 'static>;

/// The tab's per-navigation wiring for [`WorkerHost::attach`] (workers inherit
/// the document's base URL, cookie jar, User-Agent cell, DevTools queue and
/// script backend).
pub struct WorkerAttachment {
    /// Document URL: the base a relative `new Worker("/w/worker.js")` and a
    /// relative `fetch("/api/x")` resolve against.
    pub base_url: String,
    /// The tab's script-body backend (managed policy + jar + queue + archive).
    pub fetch: WorkerFetch,
    /// The tab's **private-address rule** (AUDIT finding 5, see
    /// [`crate::private_addr`]): a worker is its document's script, so its
    /// `fetch`/`XHR` may not read the browser's own network any more than the
    /// page that built it can. Judged against `base_url` (the document).
    pub private: crate::private_addr::PrivateAddressGuard,
    /// The tab's cookie jar: workers send cookies like any same-origin
    /// subresource, and Anubis-style gates that poll an authenticated endpoint
    /// from inside the worker depend on it.
    pub jar: SharedCookieJar,
    /// DevTools network queue; worker requests report with
    /// `initiator = Some("worker")`.
    pub net_events: Arc<Mutex<Vec<NetEvent>>>,
    /// Live User-Agent cell (a mid-navigation override is honoured by worker
    /// requests too, like every other request kind).
    pub user_agent: SharedUserAgent,
    /// Wakes the tab's event loop when this worker posts, so the reply lands
    /// without waiting for the next poll tick. A plain non-blocking self-pipe
    /// write is what the tab hands in.
    pub wake: Box<dyn Fn() + Send + Sync + 'static>,
}

/// One delivered message (worker → tab).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerMessage {
    /// The id this worker was started with (the document realm keys its
    /// `Worker` object by the same number).
    pub worker: u64,
    /// `"message"` (a `postMessage` reply) or `"error"` (an uncaught exception
    /// in the worker, which the spec reports as an `error` event and stops the
    /// worker).
    pub kind: String,
    /// JSON text for `"message"`, a one-line description for `"error"`.
    pub payload: String,
}

/// What the tab sends a worker. Closing is conveyed by dropping the [`Sender`]
/// (which `terminate`/`close_all` do by removing the handle), so teardown needs
/// no extra message and a worker cannot be told to close by its own script.
#[derive(Clone, Debug)]
enum Inbound {
    Message(String),
}

/// The bytes behind a `blob:` URL plus the MIME type the page declared.
#[derive(Clone, Debug)]
struct BlobRecord {
    bytes: Vec<u8>,
    content_type: String,
}

/// A live worker as the tab sees it.
struct Handle {
    id: u64,
    to_worker: Sender<Inbound>,
    terminated: Arc<AtomicBool>,
}

/// The queue every worker on this navigation posts into. Deliberately kept out
/// of [`Shared`]: the tab handle and the worker threads both hold this `Arc`
/// and nothing in it points back at the tab realm.
#[derive(Default)]
struct Bus {
    /// Messages waiting for the tab's poll loop, oldest first.
    pending: Mutex<Vec<WorkerMessage>>,
    /// The tab's wake pipe as an erased callback.
    wake: Mutex<Option<Box<dyn Fn() + Send + Sync + 'static>>>,
}

/// The attached document wiring, cloned out to worker threads.
#[derive(Clone)]
struct Attached {
    base_url: String,
    private: crate::private_addr::PrivateAddressGuard,
    fetch: WorkerFetch,
    jar: SharedCookieJar,
    net_events: Arc<Mutex<Vec<NetEvent>>>,
    user_agent: SharedUserAgent,
}

struct Shared {
    next_id: u64,
    workers: Vec<Handle>,
    blobs: HashMap<String, BlobRecord>,
    attached: Option<Attached>,
}

impl Default for Shared {
    fn default() -> Self {
        Self {
            // Ids start at 1: the JS side keeps a `live[id]` map and `0` is the
            // one value a sloppy page test (`if (worker.id)`) gets wrong.
            next_id: 1,
            workers: Vec::new(),
            blobs: HashMap::new(),
            attached: None,
        }
    }
}

/// Per-navigation Web Worker host: registry, `blob:` store and message queue.
/// Lives in [`HostState`](crate::js::HostState); cheap to clone.
#[derive(Clone)]
pub struct WorkerHost {
    shared: Arc<Mutex<Shared>>,
    bus: Arc<Bus>,
}

impl Default for WorkerHost {
    fn default() -> Self {
        Self {
            shared: Arc::new(Mutex::new(Shared::default())),
            bus: Arc::new(Bus::default()),
        }
    }
}

impl std::fmt::Debug for WorkerHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let shared = self.shared.lock().expect("js worker registry mutex");
        f.debug_struct("WorkerHost")
            .field("workers", &shared.workers.len())
            .field("blobs", &shared.blobs.len())
            .field("attached", &shared.attached.is_some())
            .finish()
    }
}

impl WorkerHost {
    /// Hand the worker host its document wiring (once per navigation, as soon
    /// as the network cells exist). Any previous attachment implies the
    /// previous document's workers, which a navigation tears down.
    pub fn attach(&self, attachment: WorkerAttachment) {
        let mut shared = self.shared.lock().expect("js worker registry mutex");
        for handle in shared.workers.drain(..) {
            // The spec closes the workers with the document that started them,
            // and their threads must not outlive the realm that made them.
            handle.terminated.store(true, Ordering::SeqCst);
        }
        let WorkerAttachment {
            base_url,
            private,
            fetch,
            jar,
            net_events,
            user_agent,
            wake,
        } = attachment;
        *self.bus.wake.lock().expect("js worker wake mutex") = Some(wake);
        shared.attached = Some(Attached {
            base_url,
            private,
            fetch,
            jar,
            net_events,
            user_agent,
        });
    }

    /// `URL.createObjectURL(blob)`: a fresh id for a new blob. Bytes start
    /// empty — the `Blob` constructor appends every part *before* asking for a
    /// URL, so a blob is complete by the time a worker can read it.
    pub fn blob_create(&self, content_type: String) -> String {
        let mut shared = self.shared.lock().expect("js worker registry mutex");
        // 122 bits of the engine's RNG source, spelled as a v4 UUID: this id is
        // the only thing separating one blob from another, and it appears inside
        // a URL a page could otherwise guess at.
        let id = uuid_from_rng();
        shared.blobs.insert(
            id.clone(),
            BlobRecord {
                bytes: Vec::new(),
                content_type,
            },
        );
        id
    }

    /// Append raw bytes to a blob (one `Blob` constructor chunk).
    pub fn blob_append(&self, id: &str, bytes: &[u8]) -> bool {
        let mut shared = self.shared.lock().expect("js worker registry mutex");
        match shared.blobs.get_mut(id) {
            Some(record) => {
                record.bytes.extend_from_slice(bytes);
                true
            }
            None => false,
        }
    }

    /// The bytes behind a blob id (`Blob.arrayBuffer()`/`Blob.text()`).
    pub fn blob_bytes(&self, id: &str) -> Option<Vec<u8>> {
        let shared = self.shared.lock().expect("js worker registry mutex");
        shared.blobs.get(id).map(|record| record.bytes.clone())
    }

    /// The MIME type a blob was created with.
    pub fn blob_type(&self, id: &str) -> Option<String> {
        let shared = self.shared.lock().expect("js worker registry mutex");
        shared
            .blobs
            .get(id)
            .map(|record| record.content_type.clone())
    }

    /// `URL.createObjectURL(blob)`: `blob:<document-origin>/<id>`. The id is the
    /// URL's last path segment, which is how [`blob_id`] finds the record again.
    pub fn object_url(&self, id: &str) -> String {
        let shared = self.shared.lock().expect("js worker registry mutex");
        let origin = shared
            .attached
            .as_ref()
            .and_then(|attached| Url::parse(&attached.base_url).ok())
            .map(|url| {
                let origin = url.origin();
                if origin.is_tuple() {
                    origin.ascii_serialization()
                } else {
                    // An opaque origin (`about:`…) has no serializable form; the
                    // spec spells it "null" and so do we.
                    "null".to_owned()
                }
            })
            .unwrap_or_default();
        format!("blob:{origin}/{id}")
    }

    /// `URL.revokeObjectURL(url)`: drop the bytes, so a later `new Worker(url)`
    /// fails the way a revoked blob does.
    pub fn revoke_object_url(&self, url: &str) -> bool {
        let Some(id) = blob_id(url) else {
            return false;
        };
        let mut shared = self.shared.lock().expect("js worker registry mutex");
        shared.blobs.remove(&id).is_some()
    }

    /// Start a worker (`new Worker(url, {type, name})`). `script_url` is a
    /// `blob:` URL the page created or a URL to fetch (resolved against the
    /// document URL). Returns the worker id the document realm keys its
    /// `Worker` object by, or the reason the worker could not start — which the
    /// glue reports as an `error` event on the object, per spec.
    pub fn start(&self, script_url: &str, name: &str, module: bool) -> Result<u64, String> {
        let mut shared = self.shared.lock().expect("js worker registry mutex");
        let attached = shared
            .attached
            .as_ref()
            .ok_or_else(|| "workers are unavailable before a document is attached".to_owned())?
            .clone();
        if module {
            // A module worker needs a real module graph inside its own realm
            // (static `import` across the thread boundary, `import.meta.url`,
            // …). Refusing by name beats failing as a `SyntaxError` on
            // `import` once the script runs.
            return Err(
                "module workers are not supported — new Worker(url, {type: \"classic\"}) works"
                    .to_owned(),
            );
        }
        let (url, source) = match blob_id(script_url) {
            Some(id) => {
                let record = shared
                    .blobs
                    .get(&id)
                    .ok_or_else(|| format!("{script_url} is not a live blob URL"))?;
                (
                    script_url.to_owned(),
                    String::from_utf8_lossy(&record.bytes).into_owned(),
                )
            }
            None => {
                let base = Url::parse(&attached.base_url)
                    .map_err(|err| format!("bad document base URL: {err}"))?;
                let absolute = base
                    .join(script_url)
                    .map_err(|err| format!("{script_url} did not resolve: {err}"))?;
                // The spec downloads the worker script in a queued task; doing
                // it here is equivalent from the page's point of view (the
                // first message can only be sent after the constructor
                // returns), and turns a failed script fetch into an `error`
                // event instead of a worker that never runs.
                let body = (attached.fetch)(&absolute)?;
                (absolute.to_string(), body)
            }
        };
        let id = shared.next_id;
        shared.next_id = shared.next_id.wrapping_add(1).max(1);
        let (sender, inbox) = mpsc::channel();
        let terminated = Arc::new(AtomicBool::new(false));
        shared.workers.push(Handle {
            id,
            to_worker: sender,
            terminated: Arc::clone(&terminated),
        });
        let thread = WorkerThread {
            id,
            name: name.to_owned(),
            url,
            source,
            attached,
            bus: Arc::clone(&self.bus),
            inbox,
            terminated,
        };
        // Spawned under the registry lock, so id assignment cannot interleave
        // with another start and a spawn failure (thread limit) leaves no
        // half-registered worker behind.
        match std::thread::Builder::new()
            .name(format!("ironsurf-worker-{id}"))
            .spawn(move || thread.run())
        {
            Ok(_) => Ok(id),
            Err(err) => {
                shared.workers.retain(|handle| handle.id != id);
                Err(format!("could not start the worker thread: {err}"))
            }
        }
    }

    /// `worker.postMessage(data)` on the tab side.
    pub fn post_to(&self, id: u64, json: String) -> bool {
        let shared = self.shared.lock().expect("js worker registry mutex");
        match shared.workers.iter().find(|handle| handle.id == id) {
            Some(handle) => handle.to_worker.send(Inbound::Message(json)).is_ok(),
            None => false,
        }
    }

    /// `worker.terminate()`: the thread sees the flag at the top of its loop,
    /// and the channel closes with the handle.
    pub fn terminate(&self, id: u64) -> bool {
        let mut shared = self.shared.lock().expect("js worker registry mutex");
        let before = shared.workers.len();
        shared.workers.retain(|handle| {
            if handle.id == id {
                handle.terminated.store(true, Ordering::SeqCst);
                false
            } else {
                true
            }
        });
        shared.workers.len() != before
    }

    /// Terminate every worker (navigation teardown / realm replacement).
    pub fn close_all(&self) {
        let mut shared = self.shared.lock().expect("js worker registry mutex");
        for handle in shared.workers.drain(..) {
            handle.terminated.store(true, Ordering::SeqCst);
        }
    }

    /// Messages posted since the last call, oldest first (the tab's poll loop
    /// hands these to the realm's delivery glue).
    pub fn take_messages(&self) -> Vec<WorkerMessage> {
        let mut pending = self.bus.pending.lock().expect("js worker message mutex");
        std::mem::take(&mut *pending)
    }

    /// Cheap "is there anything to deliver" test for the poll loop.
    #[must_use]
    pub fn has_messages(&self) -> bool {
        let pending = self.bus.pending.lock().expect("js worker message mutex");
        !pending.is_empty()
    }

    /// Live workers (diagnostics/tests).
    #[must_use]
    pub fn worker_count(&self) -> usize {
        let shared = self.shared.lock().expect("js worker registry mutex");
        shared.workers.len()
    }

    /// The document URL the host was attached with (diagnostics/tests).
    #[must_use]
    pub fn base_url(&self) -> Option<String> {
        let shared = self.shared.lock().expect("js worker registry mutex");
        shared
            .attached
            .as_ref()
            .map(|attached| attached.base_url.clone())
    }
}

/// The id inside a `blob:` URL (last path segment, minus query/fragment), or
/// `None` for a URL that is not one.
fn blob_id(url: &str) -> Option<String> {
    let rest = url.trim().strip_prefix("blob:")?;
    let tail = rest.rsplit('/').next().unwrap_or(rest);
    let tail = tail.split(['?', '#']).next().unwrap_or(tail);
    if tail.is_empty() {
        None
    } else {
        Some(tail.to_owned())
    }
}

// ---------------------------------------------------------------------------
// The worker thread

/// Everything a worker thread needs: its identity, its script, the tab's
/// network cells, the message bus and its inbox.
struct WorkerThread {
    id: u64,
    name: String,
    url: String,
    source: String,
    attached: Attached,
    bus: Arc<Bus>,
    inbox: Receiver<Inbound>,
    terminated: Arc<AtomicBool>,
}

/// Upper bound on one blocking wait, whatever the timer queue says.
const CAP: Duration = Duration::from_secs(30);

impl WorkerThread {
    /// Build the realm, run the script, then pump messages/timers until the tab
    /// closes the channel (navigation, realm teardown) or `terminate()`.
    fn run(self) {
        let WorkerThread {
            id,
            name,
            url,
            source,
            attached,
            bus,
            inbox,
            terminated,
        } = self;
        // The realm's timers live in a thread-local scope: callbacks are
        // `JsValue` handles of *this* realm (`JsValue` is not `Send`), exactly
        // like the tab realm's timer queue.
        let _scope = push_worker_scope(id, Arc::clone(&bus), AttachedCells::new(&attached));

        let mut context = Context::default();
        // The same runtime baseline the document realm gets (TextEncoder/
        // TextDecoder, atob/btoa, URL/URLSearchParams, structuredClone,
        // queueMicrotask, AbortController, console). Its *timers* are replaced
        // below with this realm's own queue: boa_runtime's need a `Runtime`
        // pump this engine does not run.
        if let Err(err) = boa_runtime::register(
            boa_runtime::extensions::ConsoleExtension::default(),
            None,
            &mut context,
        ) {
            error!("worker {id} runtime baseline failed: {err}");
        }
        register_worker_bindings(&mut context);
        crate::sha256::register(&mut context);
        // This worker's identity, before the prelude: the glue builds `location`
        // from `__ironsurfWorkerHref` as it installs, so setting these afterwards
        // would leave every worker with an empty `location`.
        for (property, value) in [
            ("__ironsurfWorkerHref", url.as_str()),
            ("__ironsurfWorkerName", name.as_str()),
        ] {
            let _ = context.global_object().set(
                js_string!(property),
                JsValue::from(js_string!(value)),
                false,
                &mut context,
            );
        }
        if let Err(err) = context.eval(Source::from_bytes(WORKER_PRELUDE.as_bytes())) {
            error!("worker {id} prelude failed to evaluate: {err}");
            return;
        }

        debug!("worker {id} running {url} ({} bytes)", source.len());
        let evaluated = context
            .eval(Source::from_bytes(source.as_bytes()).with_path(std::path::Path::new(&url)));
        if let Err(err) = evaluated {
            let message = first_line_of(&err.to_string());
            error!("worker {id} script error: {message}");
            post_to_bus(
                &bus,
                WorkerMessage {
                    worker: id,
                    kind: "error".to_owned(),
                    payload: message,
                },
            );
            let _ = context.run_jobs();
            return;
        }
        let _ = context.run_jobs();

        // The event loop: `recv` while nothing is scheduled (an idle worker
        // costs no wakeups), `recv_timeout` when a timer is pending, so a
        // message that arrives before the next deadline is handled promptly and
        // a bogus deadline cannot stall delivery.
        loop {
            if terminated.load(Ordering::SeqCst) {
                break;
            }
            match due_wait() {
                None => match inbox.recv() {
                    Ok(Inbound::Message(json)) => deliver_message(&mut context, id, &json),
                    // The tab dropped every `Sender`: `terminate` or teardown.
                    Err(_) => break,
                },
                Some(after) => match inbox.recv_timeout(after.min(CAP)) {
                    Ok(Inbound::Message(json)) => deliver_message(&mut context, id, &json),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                },
            }
            run_due_timers(&mut context);
            let _ = context.run_jobs();
        }
        let _ = context.run_jobs();
        debug!("worker {id} exited");
    }
}

/// Deliver one inbound message through the worker realm's glue. A throw is the
/// worker's own `message` handler failing: reported, loop keeps running (like a
/// browser delivering the next message after a handler threw).
fn deliver_message(context: &mut Context, id: u64, json: &str) {
    let glue = match context
        .global_object()
        .get(js_string!("__ironsurfDeliverMessage"), context)
    {
        Ok(value) => value,
        Err(err) => {
            error!("worker {id} delivery glue unreadable: {err}");
            return;
        }
    };
    let Some(callable) = glue.as_callable() else {
        error!("worker {id} has no delivery glue");
        return;
    };
    let argument = JsValue::from(js_string!(json));
    if let Err(err) = callable.call(&JsValue::undefined(), &[argument], context) {
        error!(
            "worker {id} message handler error: {}",
            first_line_of(&err.to_string())
        );
    }
}

// ---------------------------------------------------------------------------
// Worker timers (this realm's own queue, fired by this thread)

/// One scheduled worker timer.
struct Entry {
    id: u32,
    callback: JsValue,
    every: bool,
    due: Instant,
    period: Duration,
}

/// The worker realm's timer store (one per thread, see [`ACTIVE_WORKERS`]).
#[derive(Default)]
struct WorkerTimers {
    next_id: u32,
    entries: Vec<Entry>,
}

impl WorkerTimers {
    fn schedule(&mut self, callback: JsValue, delay: Duration, every: bool) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.entries.push(Entry {
            id,
            callback,
            every,
            due: Instant::now() + delay,
            period: delay,
        });
        id
    }

    fn cancel(&mut self, id: u32) -> bool {
        let before = self.entries.len();
        self.entries.retain(|entry| entry.id != id);
        self.entries.len() != before
    }

    fn earliest(&self) -> Option<Instant> {
        self.entries.iter().map(|entry| entry.due).min()
    }

    /// Everything due by `now`, earliest first; intervals are re-armed.
    fn take_due(&mut self, now: Instant) -> Vec<JsValue> {
        let entries = std::mem::take(&mut self.entries);
        let mut due: Vec<Entry> = Vec::new();
        for entry in entries {
            if entry.due <= now {
                if entry.every {
                    self.entries.push(Entry {
                        id: entry.id,
                        callback: entry.callback.clone(),
                        every: true,
                        due: now + entry.period.max(Duration::from_millis(1)),
                        period: entry.period,
                    });
                }
                due.push(entry);
            } else {
                self.entries.push(entry);
            }
        }
        due.sort_by_key(|entry| entry.due);
        due.into_iter().map(|entry| entry.callback).collect()
    }
}

/// How long the loop may block before it must look at the timer queue.
fn due_wait() -> Option<Duration> {
    let now = Instant::now();
    ACTIVE_WORKERS.with(|slot| {
        let scope = slot.borrow();
        let timers = scope.as_ref()?.timers.borrow();
        Some(timers.earliest()?.saturating_duration_since(now))
    })
}

/// Fire everything due. The caller drains microtasks afterwards.
fn run_due_timers(context: &mut Context) {
    let due: Vec<JsValue> = ACTIVE_WORKERS.with(|slot| {
        let scope = slot.borrow();
        match scope.as_ref() {
            Some(scope) => scope.timers.borrow_mut().take_due(Instant::now()),
            None => Vec::new(),
        }
    });
    for callback in due {
        let Some(function) = callback.as_callable() else {
            continue;
        };
        if let Err(err) = function.call(&JsValue::undefined(), &[], context) {
            error!(
                "worker timer callback error: {}",
                first_line_of(&err.to_string())
            );
        }
    }
}

/// This thread's worker scope. `Option` because the slot exists before the
/// scope is pushed and after it drops; the natives only run with it filled.
struct WorkerScope {
    id: u64,
    /// The message bus this worker posts replies into.
    bus: Arc<Bus>,
    /// The document cells its requests ride (jar, UA, DevTools queue, backend).
    cells: Arc<AttachedCells>,
    timers: RefCell<WorkerTimers>,
}

thread_local! {
    static ACTIVE_WORKERS: RefCell<Option<WorkerScope>> = const { RefCell::new(None) };
}

struct WorkerScopeGuard;

fn push_worker_scope(id: u64, bus: Arc<Bus>, cells: Arc<AttachedCells>) -> WorkerScopeGuard {
    ACTIVE_WORKERS.with(|slot| {
        *slot.borrow_mut() = Some(WorkerScope {
            id,
            bus,
            cells,
            timers: RefCell::new(WorkerTimers::default()),
        })
    });
    WorkerScopeGuard
}

/// This thread's worker identity + shared handles, cloned out of the scope.
/// The worker natives take this route rather than capturing `Arc`s in their
/// closures, because boa's `from_copy_closure` needs `Copy` captures and an
/// `Arc` is not `Copy` — the same reason the document realm's natives reach
/// [`HostState`](crate::js::HostState) through `js::with_host`.
fn worker_state() -> (u64, Arc<Bus>, Arc<AttachedCells>) {
    ACTIVE_WORKERS.with(|slot| {
        let slot = slot.borrow();
        let scope = slot
            .as_ref()
            .expect("worker natives only run inside a worker thread");
        (scope.id, Arc::clone(&scope.bus), Arc::clone(&scope.cells))
    })
}

impl Drop for WorkerScopeGuard {
    fn drop(&mut self) {
        ACTIVE_WORKERS.with(|slot| *slot.borrow_mut() = None);
    }
}

/// Run `body` against this thread's worker scope — the worker-thread twin of
/// `js::with_host`. Panics with no live scope, which cannot happen: these
/// natives are only registered inside a worker realm.
fn with_worker<R>(body: impl FnOnce(&mut WorkerScope) -> R) -> R {
    ACTIVE_WORKERS.with(|slot| {
        let mut slot = slot.borrow_mut();
        let scope = slot
            .as_mut()
            .expect("worker natives only run inside a worker thread");
        body(scope)
    })
}

// ---------------------------------------------------------------------------
// Message bus

/// Queue one message for the tab and poke its event loop. The wake is a
/// non-blocking pipe write, so neither side can block on the other: each holds
/// the message mutex only for the length of a `Vec` swap.
fn post_to_bus(bus: &Arc<Bus>, message: WorkerMessage) {
    debug!(
        "worker {} posts {} to the bus ({} bytes)",
        message.worker, message.kind, message.payload.len()
    );
    bus.pending
        .lock()
        .expect("js worker message mutex")
        .push(message);
    if let Some(wake) = &*bus.wake.lock().expect("js worker wake mutex") {
        wake();
    }
}

// ---------------------------------------------------------------------------
// The worker realm's network cells + exchange

/// The tab cells a worker request needs — a copy of the document's handles, not
/// a borrow of the tab realm: shared jar, shared DevTools queue, live UA cell,
/// and the script-body backend for `importScripts`.
#[derive(Clone)]
struct AttachedCells {
    fetch: WorkerFetch,
    private: crate::private_addr::PrivateAddressGuard,
    jar: SharedCookieJar,
    net_events: Arc<Mutex<Vec<NetEvent>>>,
    user_agent: SharedUserAgent,
    base_url: Arc<str>,
}

impl AttachedCells {
    fn new(attached: &Attached) -> Arc<Self> {
        Arc::new(Self {
            fetch: Arc::clone(&attached.fetch),
            private: attached.private.clone(),
            jar: Arc::clone(&attached.jar),
            net_events: Arc::clone(&attached.net_events),
            user_agent: Arc::clone(&attached.user_agent),
            base_url: Arc::from(attached.base_url.as_str()),
        })
    }

    /// Resolve a request against the **document** URL — what a classic worker's
    /// relative `fetch` uses, and what keeps path-absolute requests pointed at
    /// the site that served the page — falling back to the string as given.
    fn resolve(&self, requested: &str) -> Option<Url> {
        Url::parse(&self.base_url)
            .ok()
            .and_then(|base| base.join(requested).ok())
            .or_else(|| Url::parse(requested).ok())
    }
}

/// One exchange's outcome, as the worker realm sees it. The native answers with
/// JSON text (the realm's `fetch`/`importScripts` glue is JS), so this is the
/// wire shape of the tab's blocking client for a worker.
struct Exchange {
    status: u16,
    status_text: String,
    url: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Exchange {
    fn to_json(&self) -> String {
        let mut out = String::from("{\"ok\":");
        out.push_str(if (200..300).contains(&self.status) {
            "true"
        } else {
            "false"
        });
        out.push_str(",\"status\":");
        out.push_str(&self.status.to_string());
        out.push_str(",\"statusText\":");
        out.push_str(&json_string(&self.status_text));
        out.push_str(",\"url\":");
        out.push_str(&json_string(&self.url));
        out.push_str(",\"headers\":[");
        for (index, (name, value)) in self.headers.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push('[');
            out.push_str(&json_string(name));
            out.push(',');
            out.push_str(&json_string(value));
            out.push(']');
        }
        out.push_str("],\"body\":");
        out.push_str(&json_string(&self.body));
        out.push('}');
        out
    }
}

/// `value` as a JSON string literal. The tab's message drain uses it too,
/// so worker payloads and their kind can be spliced into a realm expression
/// without a JSON dependency in this crate.
pub(crate) fn json_string(value: &str) -> String {
    let mut out = String::from("\"");
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// One request through the tab's blocking client (the same
/// [`netmod::xhr_fetch_report`] the realm's `fetch()`/XHR use), reported to the
/// DevTools queue as a worker request.
fn exchange(
    cells: &AttachedCells,
    method: &str,
    url: &Url,
    headers: Vec<(String, String)>,
    body: Option<&[u8]>,
) -> Result<Exchange, String> {
    let events = Arc::clone(&cells.net_events);
    let report = move |mut event: NetEvent| {
        event.initiator = Some("worker".to_owned());
        events.lock().expect("net events mutex").push(event);
    };
    // AUDIT F5: the document is the worker's origin for the private-address
    // rule; a hop into the local network is refused before the request runs.
    let document = Url::parse(&cells.base_url).ok();
    cells.private.check(document.as_ref(), url, "worker")?;
    let jar = Arc::clone(&cells.jar);
    let mut jar_access: &std::sync::Mutex<_> = &jar;
    let reply = netmod::xhr_fetch_report(
        method,
        url,
        &headers,
        body,
        &mut jar_access,
        netmod::now_unix(),
        &cells.user_agent,
        Some(&report),
    )?;
    Ok(Exchange {
        status: reply.status,
        status_text: reply.status_text.clone(),
        url: reply.final_url.clone(),
        headers: reply.headers.clone(),
        body: String::from_utf8_lossy(&reply.body).into_owned(),
    })
}

// ---------------------------------------------------------------------------
// The worker realm's natives

/// The worker realm's native names.
mod native {
    /// `postMessage` payload (JSON text) out to the tab.
    pub(super) const POST: &str = "__ironsurfWorkerPost";
    /// One console line (`level`, `text`).
    pub(super) const CONSOLE: &str = "__ironsurfWorkerConsole";
    /// Schedule a callback (`callback`, `delay`, `every`).
    pub(super) const TIMER_SET: &str = "__ironsurfWorkerTimer";
    /// Cancel by id.
    pub(super) const TIMER_CLEAR: &str = "__ironsurfWorkerClear";
    /// One exchange (`url`, `method`, `headersJson`, `body`) → JSON envelope.
    pub(super) const FETCH: &str = "__ironsurfWorkerFetch";
    /// `importScripts` body (`url`) → source text.
    pub(super) const IMPORT: &str = "__ironsurfWorkerImport";
    /// `n` latin1 bytes of RNG as a JS string.
    pub(super) const RANDOM_BYTES: &str = "__ironsurfRandomBytes";
    /// A v4 UUID.
    pub(super) const RANDOM_UUID: &str = "__ironsurfRandomUUID";
    /// This worker's id.
    pub(super) const ID: &str = "__ironsurfWorkerId";
}

/// Register the worker realm's Rust natives. They reach this thread's worker
/// scope at call time ([`worker_state`]) instead of capturing handles: boa's
/// `from_copy_closure` requires `Copy` captures and every handle here is an
/// `Arc`, which is also how the document realm's natives reach
/// [`HostState`](crate::js::HostState).
fn register_worker_bindings(context: &mut Context) {
    macro_rules! native {
        ($name:expr, $len:expr, $body:expr) => {
            if let Err(err) = context.register_global_builtin_callable(
                JsString::from($name),
                $len,
                NativeFunction::from_copy_closure($body),
            ) {
                error!("js worker binding `{}` failed to install: {err}", $name);
            }
        };
    }

    native!(native::POST, 1, |_this, args, context| {
        let (id, bus, _cells) = worker_state();
        let json = string_arg(args, 0, context)?.unwrap_or_else(|| "null".to_owned());
        post_to_bus(
            &bus,
            WorkerMessage {
                worker: id,
                kind: "message".to_owned(),
                payload: json,
            },
        );
        Ok(JsValue::undefined())
    });

    native!(native::CONSOLE, 2, |_this, args, context| {
        let (id, _bus, _cells) = worker_state();
        let level = string_arg(args, 0, context)?.unwrap_or_else(|| "log".to_owned());
        let text = string_arg(args, 1, context)?.unwrap_or_default();
        match level.as_str() {
            "error" => error!("worker {id} console [error] {text}"),
            "warn" => warn!("worker {id} console [warn] {text}"),
            other => debug!("worker {id} console [{other}] {text}"),
        }
        Ok(JsValue::undefined())
    });

    native!(native::TIMER_SET, 3, |_this, args, context| {
        let callback = args.first().cloned().unwrap_or(JsValue::undefined());
        if callback.as_callable().is_none() {
            // `setTimeout(42)` schedules nothing.
            return Ok(JsValue::from(0u32));
        }
        let delay = args
            .get(1)
            .map(|value| value.to_number(context))
            .transpose()?
            .unwrap_or(0.0);
        let every = args
            .get(2)
            .map(|value| value.to_boolean())
            .unwrap_or(false);
        let delay = if delay.is_finite() && delay > 0.0 {
            Duration::from_millis(delay as u64)
        } else {
            Duration::ZERO
        };
        Ok(JsValue::from(with_worker(|scope| {
            scope.timers.borrow_mut().schedule(callback, delay, every)
        })))
    });

    native!(native::TIMER_CLEAR, 1, |_this, args, context| {
        let timer = args
            .first()
            .map(|value| value.to_u32(context))
            .transpose()?
            .unwrap_or(0);
        Ok(JsValue::from(with_worker(|scope| {
            scope.timers.borrow_mut().cancel(timer)
        })))
    });

    native!(native::FETCH, 4, |_this, args, context| {
        let (_id, _bus, cells) = worker_state();
        let requested = string_arg(args, 0, context)?.unwrap_or_default();
        let method = string_arg(args, 1, context)?
            .unwrap_or_else(|| "GET".to_owned())
            .to_uppercase();
        // Headers arrive as a JSON array of pairs; the realm's glue turns a
        // `Headers` object/record/entries `init` into that shape.
        let headers =
            pairs_from_json(&string_arg(args, 2, context)?.unwrap_or_else(|| "[]".to_owned()));
        let body = string_arg(args, 3, context)?;
        let url = cells.resolve(&requested).ok_or_else(|| {
            JsNativeError::typ().with_message(format!("{requested} did not resolve to a URL"))
        })?;
        let outcome = exchange(&cells, &method, &url, headers, body.as_deref().map(str::as_bytes))
            .map_err(|err| JsNativeError::typ().with_message(format!("fetch {url}: {err}")))?;
        Ok(JsValue::from(js_string!(outcome.to_json())))
    });

    native!(native::IMPORT, 1, |_this, args, context| {
        let (_id, _bus, cells) = worker_state();
        let requested = string_arg(args, 0, context)?.unwrap_or_default();
        let url = cells.resolve(&requested).ok_or_else(|| {
            JsNativeError::typ().with_message(format!("{requested} did not resolve to a URL"))
        })?;
        // `importScripts` goes through the tab's script-body backend (managed
        // policy, jar, redirects, DevTools queue), like a `<script src>`.
        let source = (cells.fetch)(&url)
            .map_err(|err| JsNativeError::typ().with_message(format!("importScripts: {err}")))?;
        Ok(JsValue::from(js_string!(source)))
    });

    native!(native::RANDOM_BYTES, 1, |_this, args, context| {
        let len = args
            .first()
            .map(|value| value.to_number(context))
            .transpose()?
            .unwrap_or(0.0);
        // Bounded: a page asking for 4 GB of RNG gets the cap (this is a browsing
        // engine's id supply, not a KDF).
        let len = (len.max(0.0) as usize).min(64 * 1024);
        let mut codes = Vec::with_capacity(len);
        for _ in 0..len {
            codes.push((next_random_u64() & 0xFF) as u16);
        }
        Ok(JsValue::from(JsString::from(String::from_utf16_lossy(&codes))))
    });

    native!(native::RANDOM_UUID, 0, |_this, _args, _context| {
        Ok(JsValue::from(js_string!(uuid_from_rng())))
    });

    native!(native::ID, 0, |_this, _args, _context| {
        Ok(JsValue::from(worker_state().0 as f64))
    });
}

/// A v4 UUID from the engine's RNG source (blob ids and `crypto.randomUUID`).
fn uuid_from_rng() -> String {
    let (a, b) = (next_random_u64(), next_random_u64());
    format!(
        "{:08x}-{:04x}-4{:03x}-{:04x}-{:012x}",
        a as u32,
        ((a >> 32) & 0xffff) as u16,
        ((b >> 48) & 0x0fff) as u16,
        (((b >> 32) as u16) & 0x3fff) | 0x8000,
        b & 0x0000_ffff_ffff_ffff,
    )
}

/// The worker id argument of the document-side natives. A worker id is a plain
/// JS number; anything that is not a positive finite number addresses no worker
/// (and a bad id is a no-op on the Rust side, not a throw).
fn worker_id_arg(args: &[JsValue], context: &mut Context) -> JsResult<u64> {
    let raw = args
        .first()
        .map(|value| value.to_number(context))
        .transpose()?
        .unwrap_or(0.0);
    if raw.is_finite() && raw > 0.0 {
        Ok(raw as u64)
    } else {
        Ok(0)
    }
}

/// `args[index]` as a string (absent/`undefined`/`null` → `None`).
fn string_arg(args: &[JsValue], index: usize, context: &mut Context) -> JsResult<Option<String>> {
    let Some(value) = args.get(index) else {
        return Ok(None);
    };
    if value.is_undefined() || value.is_null() {
        return Ok(None);
    }
    Ok(Some(value.to_string(context)?.to_std_string_escaped()))
}

/// The JSON-array-of-pairs reader the `fetch` native needs (this crate has no
/// JSON dep): `[["name","value"],…]`; anything else reads as no headers.
fn pairs_from_json(text: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut pair: Vec<String> = Vec::new();
    let mut in_string = false;
    let mut escaping = false;
    for ch in text.chars() {
        if escaping {
            current.push(match ch {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                other => other,
            });
            escaping = false;
            continue;
        }
        match ch {
            '\\' if in_string => escaping = true,
            '"' if in_string => {
                pair.push(std::mem::take(&mut current));
                in_string = false;
            }
            '"' => in_string = true,
            ']' => {
                if pair.len() == 2 {
                    let mut items = pair.drain(..);
                    let name = items.next().unwrap_or_default();
                    let value = items.next().unwrap_or_default();
                    out.push((name, value));
                }
            }
            _ if in_string => current.push(ch),
            _ => {}
        }
    }
    out
}

/// A one-line description from a boa error's Display (which appends a shadow
/// stack).
fn first_line_of(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_owned()
}

// ---------------------------------------------------------------------------
// The document realm's side: Worker / Blob / blob: URLs

/// The document realm's native names (installed by [`register_main_bindings`]).
mod main_native {
    /// `(scriptUrl, name, module)` → worker id, or a thrown `Error`.
    pub(super) const START: &str = "__ironsurfWorkerStart";
    /// `(id, json)` → bool.
    pub(super) const POST_TO: &str = "__ironsurfWorkerPostTo";
    /// `(id)` → bool.
    pub(super) const TERMINATE: &str = "__ironsurfWorkerTerminate";
    /// `(contentType)` → blob id.
    pub(super) const BLOB_CREATE: &str = "__ironsurfBlobCreate";
    /// `(id, bytesAsLatin1)` → bool.
    pub(super) const BLOB_APPEND: &str = "__ironsurfBlobAppend";
    /// `(id)` → bytes as a latin1 JS string.
    pub(super) const BLOB_BYTES: &str = "__ironsurfBlobBytes";
    /// `(id)` → declared MIME type.
    pub(super) const BLOB_TYPE: &str = "__ironsurfBlobType";
    /// `(id)` → `blob:<origin>/<id>`.
    pub(super) const OBJECT_URL: &str = "__ironsurfObjectURL";
    /// `(url)` → bool.
    pub(super) const REVOKE: &str = "__ironsurfRevokeObjectURL";
}

/// Install the document realm's worker/blob natives. The prelude's
/// `Worker`/`Blob`/`URL.createObjectURL` glue (appended to the realm prelude in
/// `js.rs` as [`WORKER_PRELUDE_MAIN`]) calls these. Like every other realm
/// native they take no captured state: they resolve this realm's
/// [`WorkerHost`] through `js`'s active-scope stack
/// ([`crate::js::active_workers`]), because boa's `from_copy_closure` needs
/// `Copy` captures.
pub fn register_main_bindings(context: &mut Context) {
    macro_rules! native {
        ($name:expr, $len:expr, $body:expr) => {
            if let Err(err) = context.register_global_builtin_callable(
                JsString::from($name),
                $len,
                NativeFunction::from_copy_closure($body),
            ) {
                error!("js binding `{}` failed to install: {err}", $name);
            }
        };
    }

    macro_rules! host {
        () => {
            crate::js::active_workers().ok_or_else(|| {
                JsNativeError::error().with_message("no document realm is active")
            })?
        };
    }

    native!(main_native::START, 3, |_this, args, context| {
        let host = host!();
        let script_url = string_arg(args, 0, context)?.unwrap_or_default();
        let name = string_arg(args, 1, context)?.unwrap_or_default();
        let module = args
            .get(2)
            .map(|value| value.to_boolean())
            .unwrap_or(false);
        match host.start(&script_url, &name, module) {
            // The id is a JS number the prelude keys its `Worker` object with;
            // every later call comes back through it.
            Ok(id) => Ok(JsValue::from(id as f64)),
            Err(reason) => Err(JsNativeError::error()
                .with_message(format!("SecurityError: {reason}"))
                .into()),
        }
    });

    native!(main_native::POST_TO, 2, |_this, args, context| {
        let host = host!();
        let id = worker_id_arg(args, context)?;
        let json = string_arg(args, 1, context)?.unwrap_or_else(|| "null".to_owned());
        Ok(JsValue::from(host.post_to(id, json)))
    });

    native!(main_native::TERMINATE, 1, |_this, args, context| {
        let host = host!();
        let id = worker_id_arg(args, context)?;
        Ok(JsValue::from(host.terminate(id)))
    });

    native!(main_native::BLOB_CREATE, 1, |_this, args, context| {
        let host = host!();
        let content_type = string_arg(args, 0, context)?.unwrap_or_default();
        Ok(JsValue::from(js_string!(host.blob_create(content_type))))
    });

    native!(main_native::BLOB_APPEND, 2, |_this, args, context| {
        let host = host!();
        let id = string_arg(args, 0, context)?.unwrap_or_default();
        // The bytes cross as a JS string, one latin1 code unit per byte (what
        // `String.fromCharCode` produces and what `Uint8Array` round-trips
        // losslessly).
        let bytes = string_arg(args, 1, context)?.unwrap_or_default();
        let buffer: Vec<u8> = bytes.encode_utf16().map(|unit| (unit & 0xFF) as u8).collect();
        Ok(JsValue::from(host.blob_append(&id, &buffer)))
    });

    native!(main_native::BLOB_BYTES, 1, |_this, args, context| {
        let host = host!();
        let id = string_arg(args, 0, context)?.unwrap_or_default();
        match host.blob_bytes(&id) {
            Some(bytes) => {
                let codes: Vec<u16> = bytes.iter().map(|byte| u16::from(*byte)).collect();
                Ok(JsValue::from(JsString::from(String::from_utf16_lossy(&codes))))
            }
            None => Err(JsNativeError::error()
                .with_message("NotFoundError: no blob for that id")
                .into()),
        }
    });

    native!(main_native::BLOB_TYPE, 1, |_this, args, context| {
        let host = host!();
        let id = string_arg(args, 0, context)?.unwrap_or_default();
        Ok(JsValue::from(js_string!(
            host.blob_type(&id).unwrap_or_default()
        )))
    });

    native!(main_native::OBJECT_URL, 1, |_this, args, context| {
        let host = host!();
        let id = string_arg(args, 0, context)?.unwrap_or_default();
        Ok(JsValue::from(js_string!(host.object_url(&id))))
    });

    native!(main_native::REVOKE, 1, |_this, args, context| {
        let host = host!();
        let url = string_arg(args, 0, context)?.unwrap_or_default();
        Ok(JsValue::from(host.revoke_object_url(&url)))
    });
}

/// The document realm's worker glue, appended to the realm prelude in `js.rs`
/// after the page-facing bindings (it only adds names: `Blob`,
/// `URL.createObjectURL`/`revokeObjectURL`, `Worker`,
/// `__ironsurfDeliverWorkerMessages`).
///
/// Byte chunks cross to Rust as latin1 strings (one code unit per byte), which
/// is why `Blob` encodes with `TextEncoder` first: the spec stores the UTF-8
/// bytes of a string chunk, and a worker then reads the script back as text.
pub const WORKER_PRELUDE_MAIN: &str = r##"
(function (host) {
  var encoder = new TextEncoder();

  function bytesOf(chunk) {
    if (typeof chunk === "string") return encoder.encode(chunk);
    if (typeof Blob === "function" && chunk instanceof Blob) return chunk.__bytes;
    if (typeof ArrayBuffer !== "undefined" &&
        (chunk instanceof ArrayBuffer || (typeof ArrayBuffer.isView === "function" && ArrayBuffer.isView(chunk)))) {
      return new Uint8Array(chunk.buffer === undefined ? chunk : chunk.buffer,
                            chunk.byteOffset || 0,
                            chunk.byteLength === undefined ? chunk.length : chunk.byteLength);
    }
    if (Array.isArray(chunk)) {
      var parts = chunk.map(bytesOf), total = 0, out;
      parts.forEach(function (part) { total += part.length; });
      out = new Uint8Array(total);
      var at = 0;
      parts.forEach(function (part) { out.set(part, at); at += part.length; });
      return out;
    }
    // Anything else stringifies, like the spec's fallback for a non-BufferSource.
    return encoder.encode(String(chunk));
  }

  // latin1 (one code unit per byte) for the Rust hop, built in slices because
  // `String.fromCharCode` takes a bounded argument list.
  function toLatin1(bytes) {
    var out = "", at = 0, size = 4096;
    while (at < bytes.length) {
      var slice = bytes.subarray(at, Math.min(at + size, bytes.length));
      out += String.fromCharCode.apply(String, Array.prototype.slice.call(slice));
      at += size;
    }
    return out;
  }
  function fromLatin1(text) {
    var out = new Uint8Array(text.length);
    for (var i = 0; i < text.length; i++) out[i] = text.charCodeAt(i) & 0xFF;
    return out;
  }

  // ---- Blob ---------------------------------------------------------
  function Blob(chunks, options) {
    if (!(this instanceof Blob)) throw new TypeError("Failed to construct 'Blob': use 'new'");
    var list = chunks === undefined ? [] : (Array.isArray(chunks) ? chunks : [chunks]);
    var parts = list.map(bytesOf), total = 0;
    parts.forEach(function (part) { total += part.length; });
    var bytes = new Uint8Array(total), at = 0;
    parts.forEach(function (part) { bytes.set(part, at); at += part.length; });
    var type = (options && options.type) ? String(options.type) : "";
    // Rust owns the bytes: a worker's `new Worker(blobUrl)` reads them back on
    // its own thread, and `revokeObjectURL` drops them.
    var id = host.__ironsurfBlobCreate(type);
    if (total) host.__ironsurfBlobAppend(id, toLatin1(bytes));
    Object.defineProperties(this, {
      __id: { value: id },
      __bytes: { value: bytes },
      size: { value: total },
      type: { value: type }
    });
  }
  Blob.prototype.text = function () {
    try {
      var fresh = fromLatin1(host.__ironsurfBlobBytes(this.__id));
      return Promise.resolve(new TextDecoder().decode(fresh));
    } catch (e) {
      return Promise.resolve(new TextDecoder().decode(this.__bytes));
    }
  };
  Blob.prototype.arrayBuffer = function () {
    var self = this;
    return self.text().then(function (text) {
      var encoded = encoder.encode(text);
      return encoded.buffer.slice(0, encoded.byteLength);
    });
  };
  Blob.prototype.slice = function (start, end) {
    var all = this.__bytes;
    var from = start === undefined ? 0 : (start < 0 ? Math.max(all.length + start, 0) : Math.min(start, all.length));
    var to = end === undefined ? all.length : (end < 0 ? Math.max(all.length + end, from) : Math.min(end, all.length));
    if (to < from) to = from;
    return new Blob([all.slice(from, to)]);
  };
  Blob.prototype.constructor = Blob;
  host.Blob = Blob;

  // ---- URL.createObjectURL / revokeObjectURL ------------------------
  // `URL` itself stays boa_runtime's (the WHATWG parser); these two statics are
  // DOM-side, so they are added to it rather than replacing it.
  var alive = Object.create(null);
  function addUrlStatic(name, value) {
    if (typeof host.URL === "function" || (host.URL && typeof host.URL === "object")) {
      try { Object.defineProperty(host.URL, name, { value: value, writable: true, configurable: true }); }
      catch (e) { host.URL[name] = value; }
    }
  }
  addUrlStatic("createObjectURL", function (blob) {
    if (!(blob instanceof Blob)) {
      // `MediaSource` and friends are not implemented: saying so beats handing
      // back a URL that resolves to nothing.
      throw new TypeError("URL.createObjectURL: IronSurf implements Blob only");
    }
    var url = host.__ironsurfObjectURL(blob.__id);
    alive[url] = blob;
    return url;
  });
  addUrlStatic("revokeObjectURL", function (url) {
    var text = String(url);
    delete alive[text];
    return host.__ironsurfRevokeObjectURL(text);
  });

  // ---- the event-target shape both sides share ----------------------
  function eventTarget(proto) {
    proto.addEventListener = function (type, callback) {
      if (typeof callback !== "function") return;
      var key = "__handlers_" + type;
      if (!this[key]) this[key] = [];
      if (this[key].indexOf(callback) === -1) this[key].push(callback);
    };
    proto.removeEventListener = function (type, callback) {
      var entries = this["__handlers_" + type];
      if (!entries) return;
      var at = entries.indexOf(callback);
      if (at !== -1) entries.splice(at, 1);
    };
    proto.dispatchEvent = function (event) {
      if (!event || typeof event.type !== "string") return false;
      var handler = this["on" + event.type];
      if (typeof handler === "function") {
        try { handler.call(this, event); } catch (e) { if (typeof reportError === "function") reportError(e); }
      }
      var entries = (this["__handlers_" + event.type] || []).slice();
      for (var i = 0; i < entries.length; i++) {
        try { entries[i].call(this, event); } catch (e) { if (typeof reportError === "function") reportError(e); }
      }
      return true;
    };
    return proto;
  }

  // ---- Worker -------------------------------------------------------
  var live = Object.create(null);

  function Worker(scriptURL, options) {
    if (!(this instanceof Worker)) throw new TypeError("Failed to construct 'Worker': use 'new'");
    var init = options || {};
    var module = String(init.type || "classic") === "module";
    var name = init.name === undefined || init.name === null ? "" : String(init.name);
    var requested = String(scriptURL);
    var self = this;
    this.id = null;
    this.onmessage = null;
    this.onerror = null;
    this.onmessageerror = null;
    this.terminated = false;
    this.queue = [];
    var origin = "";
    try { origin = new URL(String(host.location.href)).origin; } catch (e) { origin = ""; }
    this.origin = origin;
    // The script starts now; the id is known immediately so a `postMessage`
    // issued in the same turn is buffered on the worker's channel (which is
    // also what the spec's queued task delivers). Failures arrive as an async
    // `error` event, so `w.onerror = …` after the constructor still sees them.
    var id;
    try {
      id = host.__ironsurfWorkerStart(requested, name, module);
    } catch (e) {
      queueMicrotask(function () {
        self.dispatchEvent({
          type: "error",
          message: String((e && e.message) || e),
          filename: requested,
          lineno: 0,
          colno: 0,
          bubbles: false,
          cancelable: false
        });
      });
      return;
    }
    this.id = id;
    live[id] = this;
    while (this.queue.length) host.__ironsurfWorkerPostTo(id, this.queue.shift());
  }
  eventTarget(Worker.prototype);
  Worker.prototype.postMessage = function (data) {
    // The second argument (the transfer list) is accepted and ignored: this
    // engine has no live `MessagePort` to hand over and delivered `ports` are
    // always empty — see the structured-clone note in worker.rs.
    if (this.terminated) return;
    var json = JSON.stringify(data === undefined ? null : data);
    if (json === undefined) json = "null";
    if (this.id === null) { this.queue.push(json); return; }
    host.__ironsurfWorkerPostTo(this.id, json);
  };
  Worker.prototype.terminate = function () {
    this.terminated = true;
    if (this.id === null) return;
    host.__ironsurfWorkerTerminate(this.id);
    delete live[this.id];
  };
  host.Worker = Worker;

  // The poll loop hands over `JSON.stringify([{worker,kind,payload},…])`; the
  // reply for a worker terminated mid-batch is dropped, and messages keep their
  // order within a worker.
  host.__ironsurfDeliverWorkerMessages = function (envelope) {
    var batch;
    try { batch = JSON.parse(envelope); } catch (e) { return 0; }
    if (!batch || !batch.length) return 0;
    for (var i = 0; i < batch.length; i++) {
      var item = batch[i];
      var target = live[item.worker];
      if (!target || target.terminated) continue;
      var event;
      if (item.kind === "error") {
        event = { type: "error", message: String(item.payload), filename: "", lineno: 0, colno: 0 };
      } else {
        var data;
        try { data = JSON.parse(item.payload); } catch (e2) { data = item.payload; }
        event = {
          type: "message",
          data: data,
          origin: target.origin,
          lastEventId: "",
          source: null,
          ports: []
        };
      }
      event.stopPropagation = function () {};
      event.preventDefault = function () {};
      target.dispatchEvent(event);
    }
    return batch.length;
  };
})(globalThis);
"##;

/// The worker realm's own glue — the document prelude never runs here, so this
/// is the whole classic-worker surface: the scope as an event target, `self`,
/// `postMessage` (JSON), `importScripts`, the realm's own timers, `console`,
/// `crypto`, `location`/`name`, and a `fetch` on the tab's network stack.
///
/// Deliberately **not** provided (a `ReferenceError` is honest, a stub that
/// silently drops data is not): `MessageChannel`/`MessagePort`/
/// `BroadcastChannel` (see the structured-clone note in this module's header),
/// `document`/`window` (there is no DOM in a worker), `WebSocket` (no socket
/// transport in this engine at all), `URL.createObjectURL` (blobs live in the
/// document), `Notification`, `ServiceWorker`/`SharedWorker` (no persistent
/// origin), `importScripts` of cross-origin scripts without CORS handling (the
/// tab's policy answers instead).
pub const WORKER_PRELUDE: &str = r##"
(function (host) {
  // The worker global scope: `self` is the canonical name for it.
  host.self = host;

  if (typeof host.reportError !== "function") {
    host.reportError = function (e) {
      host.__ironsurfWorkerConsole("error", String((e && e.message) || e));
    };
  }

  // ---- the scope is an event target --------------------------------
  var listeners = Object.create(null);
  host.addEventListener = function (type, callback) {
    if (typeof callback !== "function") return;
    var key = String(type);
    if (!listeners[key]) listeners[key] = [];
    if (listeners[key].indexOf(callback) === -1) listeners[key].push(callback);
  };
  host.removeEventListener = function (type, callback) {
    var entries = listeners[String(type)];
    if (!entries) return;
    var at = entries.indexOf(callback);
    if (at !== -1) entries.splice(at, 1);
  };
  host.dispatchEvent = function (event) {
    if (!event || typeof event.type !== "string") return false;
    var handler = host["on" + event.type];
    if (typeof handler === "function") {
      try { handler.call(host, event); } catch (e) { host.reportError(e); }
    }
    var entries = (listeners[event.type] || []).slice();
    for (var i = 0; i < entries.length; i++) {
      try { entries[i].call(host, event); } catch (e) { host.reportError(e); }
    }
    return true;
  };
  host.onmessage = null;
  host.onerror = null;
  host.onmessageerror = null;

  // ---- postMessage: a JSON projection of structured clone ----------
  function serialize(value) {
    var json = JSON.stringify(value === undefined ? null : value);
    return json === undefined ? "null" : json;
  }
  host.postMessage = function (data) {
    // The transfer list argument is accepted and ignored: there is no live port
    // to transfer here and `ports` is always empty.
    host.__ironsurfWorkerPost(serialize(data));
  };

  // The worker thread calls this for every queued message.
  host.__ironsurfDeliverMessage = function (json) {
    var data;
    try { data = JSON.parse(json); } catch (e) { data = json; }
    var event = {
      type: "message",
      data: data,
      origin: host.location.origin,
      lastEventId: "",
      source: null,
      ports: [],
      stopPropagation: function () {},
      preventDefault: function () {}
    };
    host.dispatchEvent(event);
  };

  // ---- timers -----------------------------------------------------
  // This realm's own queue (boa_runtime's timers need a `Runtime` pump this
  // engine does not run); the worker thread fires what is due.
  host.setTimeout = function (callback, delay) {
    return host.__ironsurfWorkerTimer(callback, delay, false);
  };
  host.setInterval = function (callback, delay) {
    return host.__ironsurfWorkerTimer(callback, delay, true);
  };
  host.clearTimeout = function (id) { host.__ironsurfWorkerClear(id); };
  host.clearInterval = function (id) { host.__ironsurfWorkerClear(id); };

  // ---- console ----------------------------------------------------
  function stringify(value) {
    if (typeof value === "string") return value;
    if (value === undefined) return "undefined";
    if (value === null) return "null";
    try {
      var json = JSON.stringify(value);
      return json === undefined ? String(value) : json;
    } catch (e) {
      return String(value);
    }
  }
  var console = {};
  ["log", "info", "debug", "trace", "dir", "warn", "error"].forEach(function (level) {
    console[level] = function () {
      var parts = [];
      for (var i = 0; i < arguments.length; i++) parts.push(stringify(arguments[i]));
      host.__ironsurfWorkerConsole(level, parts.join(" "));
    };
  });
  console.assert = function (condition) {
    if (condition) return;
    var parts = ["Assertion failed"];
    for (var i = 1; i < arguments.length; i++) parts.push(stringify(arguments[i]));
    host.__ironsurfWorkerConsole("error", parts.join(" "));
  };
  console.table = console.log;
  console.group = console.groupCollapsed = console.groupEnd = function () {};
  console.time = console.timeEnd = function () {};
  host.console = console;

  // ---- crypto -----------------------------------------------------
  function bytesFrom(source) {
    if (typeof source === "string") return new Uint8Array(encoder.encode(source));
    if (source instanceof ArrayBuffer || ArrayBuffer.isView(source)) {
      return new Uint8Array(source.buffer === undefined ? source : source.buffer,
                            source.byteOffset || 0,
                            source.byteLength === undefined ? source.length : source.byteLength);
    }
    return new Uint8Array(0);
  }
  var encoder = new TextEncoder();
  host.crypto = {
    getRandomValues: function (target) {
      if (!target || typeof target.length !== "number") return target;
      var view = target instanceof Uint8Array
        ? target
        : new Uint8Array(target.buffer, target.byteOffset || 0, target.length);
      var bytes = host.__ironsurfRandomBytes(view.length);
      for (var i = 0; i < view.length; i++) view[i] = bytes.charCodeAt(i) & 0xFF;
      return target;
    },
    randomUUID: function () { return host.__ironsurfRandomUUID(); },
    subtle: {
      digest: function (algorithm, data) {
        return new Promise(function (resolve, reject) {
          var name = String(algorithm && algorithm.name ? algorithm.name : algorithm);
          if (name.toUpperCase().replace(/-/g, "") !== "SHA256") {
            reject(new Error("NotSupportedError: IronSurf implements crypto.subtle.digest for SHA-256 only"));
            return;
          }
          var raw = bytesFrom(data), codes = "";
          for (var i = 0; i < raw.length; i++) codes += String.fromCharCode(raw[i]);
          // mode 1 = latin1: the string carries the source bytes exactly.
          var digest = host.__ironsurfSha256(codes, 1);
          var out = new Uint8Array(digest.length);
          for (var j = 0; j < digest.length; j++) out[j] = digest.charCodeAt(j) & 0xFF;
          resolve(out.buffer);
        });
      }
    }
  };

  // ---- location, name --------------------------------------------
  var href = host.__ironsurfWorkerHref || "";
  var location = {
    href: href,
    ancestorOrigins: { length: 0, contains: function () { return false; }, item: function () { return ""; } },
    reload: function () {},
    replace: function () {},
    assign: function () {}
  };
  try {
    var parsed = new URL(href);
    location.protocol = parsed.protocol;
    location.host = parsed.host;
    location.hostname = parsed.hostname;
    location.port = parsed.port;
    location.pathname = parsed.pathname;
    location.search = parsed.search;
    location.hash = parsed.hash;
    location.origin = parsed.origin;
  } catch (e) {
    location.protocol = ""; location.host = ""; location.hostname = ""; location.port = "";
    location.pathname = href; location.search = ""; location.hash = ""; location.origin = "";
  }
  host.location = location;
  host.name = host.__ironsurfWorkerName || "";

  // ---- importScripts ---------------------------------------------
  host.importScripts = function () {
    for (var i = 0; i < arguments.length; i++) {
      var source = host.__ironsurfWorkerImport(String(arguments[i]));
      // Indirect eval: the body runs in the worker's global scope, which is
      // what a classic `importScripts` does.
      (0, eval)(source);
    }
  };

  // ---- fetch ------------------------------------------------------
  // The tab's blocking network stack behind a JSON envelope: same cookie jar,
  // same User-Agent, same DevTools queue (as `initiator = "worker"`). A
  // subset by construction: no `ReadableStream` body, no `Request`/`Response`
  // classes, no `AbortSignal` — the exchange finishes before the promise
  // settles, and a response body is text (bytes via `arrayBuffer`).
  function headerPairs(init) {
    if (!init) return [];
    if (typeof init.forEach === "function" && typeof init.entries === "function") {
      var collected = [];
      init.forEach(function (value, key) { collected.push([String(key), String(value)]); });
      return collected;
    }
    var out = [];
    Object.keys(init).forEach(function (key) { out.push([key, String(init[key])]); });
    return out;
  }
  host.fetch = function (resource, init) {
    var options = init || {};
    var url = typeof resource === "string" ? resource : String((resource && resource.url) || resource);
    var method = options.method ? String(options.method) : "GET";
    var headers = headerPairs(options.headers);
    var body = options.body === undefined || options.body === null ? null : String(options.body);
    return new Promise(function (resolve, reject) {
      var envelope;
      try {
        envelope = JSON.parse(host.__ironsurfWorkerFetch(url, method, JSON.stringify(headers), body));
      } catch (e) {
        reject(new TypeError("Failed to fetch: " + String((e && e.message) || e)));
        return;
      }
      var responseHeaders = {
        get: function (name) {
          var wanted = String(name).toLowerCase(), found = null;
          (envelope.headers || []).forEach(function (pair) {
            if (pair[0].toLowerCase() === wanted && found === null) found = pair[1];
          });
          return found;
        },
        has: function (name) {
          var wanted = String(name).toLowerCase();
          return (envelope.headers || []).some(function (pair) { return pair[0].toLowerCase() === wanted; });
        },
        entries: function () { return (envelope.headers || []).slice(); },
        forEach: function (fn) {
          (envelope.headers || []).forEach(function (pair) { fn(pair[1], pair[0], responseHeaders); });
        }
      };
      var response = {
        ok: !!envelope.ok,
        status: envelope.status,
        statusText: envelope.statusText || "",
        url: envelope.url || url,
        redirected: false,
        type: "basic",
        headers: responseHeaders,
        bodyUsed: false,
        text: function () { return Promise.resolve(envelope.body || ""); },
        json: function () {
          try { return Promise.resolve(JSON.parse(envelope.body || "null")); }
          catch (e) { return Promise.reject(new SyntaxError("response body is not JSON")); }
        },
        arrayBuffer: function () {
          var raw = envelope.body || "", out = new Uint8Array(raw.length);
          for (var i = 0; i < raw.length; i++) out[i] = raw.charCodeAt(i) & 0xFF;
          return Promise.resolve(out.buffer);
        }
      };
      resolve(response);
    });
  };
})(globalThis);
"##;

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;

    /// A localhost HTTP server that answers a fixed path → body table, so a
    /// worker's script body, its `importScripts` body and its `fetch` body all
    /// come from the test (never the internet). Every answer is `200` and
    /// `text/javascript`, which the script backend accepts for all three.
    struct ToyServer {
        port: u16,
        /// Kept so the thread lives as long as the test; joins on drop.
        _thread: std::thread::JoinHandle<()>,
    }

    impl ToyServer {
        fn start(routes: &[(&str, &str)]) -> ToyServer {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind localhost");
            let port = listener.local_addr().expect("local addr").port();
            let table: Vec<(String, String)> = routes
                .iter()
                .map(|(path, body)| (path.to_string(), body.to_string()))
                .collect();
            let thread = std::thread::spawn(move || {
                // Bounded: the server stops after enough requests to outlive the
                // test, so a leaked listener cannot pin a port for the run.
                for stream in listener.incoming().take(64) {
                    let Ok(mut stream) = stream else { continue };
                    let mut buffer = [0u8; 4096];
                    let read = stream.read(&mut buffer).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                    let path = request
                        .split_whitespace()
                        .nth(1)
                        .unwrap_or("/")
                        .to_owned();
                    let body = table
                        .iter()
                        .find(|(wanted, _)| path.starts_with(wanted.as_str()))
                        .map(|(_, body)| body.clone())
                        .unwrap_or_default();
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/javascript\r\nContent-Length: \
                         {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes());
                    let _ = stream.flush();
                }
            });
            ToyServer {
                port,
                _thread: thread,
            }
        }

        /// Document URL a page at `/page/main.html` would have.
        fn document_url(&self) -> String {
            format!("http://127.0.0.1:{}/page/main.html", self.port)
        }
    }

    /// Wire `host` to a document whose script backend is the toy server, with
    /// the same cell types the tab passes (jar, UA cell, DevTools queue) and a
    /// wake hook that counts calls.
    fn attach_document(host: &WorkerHost, server: &ToyServer) -> Arc<Mutex<usize>> {
        let port = server.port;
        let wakes = Arc::new(Mutex::new(0usize));
        let document = server.document_url();
        host.attach(WorkerAttachment {
            base_url: document,
            private: crate::private_addr::PrivateAddressGuard::default(),
            // The tab's script-body contract, answered by the toy server with a
            // blocking localhost GET: what `fetch_script_body` gives the real
            // tab, minus the policy, the jar and the DevTools queue.
            fetch: Arc::new(move |url: &Url| toy_fetch(port, url)),
            
            jar: Arc::new(Mutex::new(ironsurf_storage::CookieJar::new())),
            net_events: Arc::new(Mutex::new(Vec::new())),
            user_agent: Arc::new(Mutex::new("IronSurf-worker-test".to_owned())),
            wake: {
                let wakes = Arc::clone(&wakes);
                Box::new(move || {
                    *wakes.lock().expect("wake counter") += 1;
                })
            },
        });
        wakes
    }

    /// One blocking HTTP GET against [`ToyServer`], returning the body.
    fn toy_fetch(port: u16, url: &Url) -> Result<String, String> {
        use std::io::{Read, Write};
        let address = format!("127.0.0.1:{port}");
        let mut stream = std::net::TcpStream::connect(&address).map_err(|err| err.to_string())?;
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n",
            url.path()
        );
        stream
            .write_all(request.as_bytes())
            .map_err(|err| err.to_string())?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|err| err.to_string())?;
        let text = String::from_utf8_lossy(&raw).into_owned();
        text.split_once("\r\n\r\n")
            .map(|(_, body)| body.to_owned())
            .ok_or_else(|| "toy server sent no header terminator".to_owned())
    }

    /// Read `payload` as JSON (worker payloads are JSON text).    /// Read `payload` as JSON (worker payloads are JSON text).
    fn json(payload: &str) -> serde_json::Value {
        serde_json::from_str(payload).unwrap_or_else(|_| panic!("not JSON: {payload}"))
    }

    /// Everything the bus has delivered so far, kept across waits. A batch can
    /// carry several messages at once, so a wait that simply dropped what it
    /// was not looking for would lose the messages a later wait needs.
    struct Inbox<'a> {
        host: &'a WorkerHost,
        seen: Vec<WorkerMessage>,
    }

    impl<'a> Inbox<'a> {
        fn new(host: &'a WorkerHost) -> Inbox<'a> {
            Inbox {
                host,
                seen: Vec::new(),
            }
        }

        /// Every message seen so far, oldest first.
        fn seen(&self) -> &Vec<WorkerMessage> {
            &self.seen
        }

        /// Wait until some delivered message matches, or fail after a while. A
        /// worker thread is asynchronous by nature; the wait is bounded so a
        /// broken worker fails the test instead of hanging it.
        fn wait_for(&mut self, matches: impl Fn(&WorkerMessage) -> bool) -> WorkerMessage {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if let Some(found) = self.seen.iter().find(|message| matches(message)) {
                    return found.clone();
                }
                self.seen.extend(self.host.take_messages());
                assert!(
                    Instant::now() < deadline,
                    "worker never produced the expected message; saw {:?}",
                    self.seen
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }

    #[test]
    fn blob_store_round_trips_bytes_and_object_urls() {
        let host = WorkerHost::default();
        host.attach(WorkerAttachment {
            base_url: "https://example.test/page/main.html".to_owned(),
            private: crate::private_addr::PrivateAddressGuard::default(),
            fetch: Arc::new(|url: &Url| Err(format!("no bodies here: {url}"))),
            jar: Arc::new(Mutex::new(ironsurf_storage::CookieJar::new())),
            net_events: Arc::new(Mutex::new(Vec::new())),
            user_agent: Arc::new(Mutex::new("IronSurf-test".to_owned())),
            wake: Box::new(|| {}),
        });

        let id = host.blob_create("text/plain;charset=UTF-8".to_owned());
        assert!(host.blob_append(&id, b"h\xc3\xa9llo"));
        assert!(host.blob_append(&id, b"!"));
        // Appends concatenate; `size` is bytes, not code units.
        assert_eq!(host.blob_bytes(&id).expect("stored bytes"), b"h\xc3\xa9llo!");
        assert_eq!(
            host.blob_type(&id).as_deref(),
            Some("text/plain;charset=UTF-8")
        );

        let url = host.object_url(&id);
        assert_eq!(url, format!("blob:https://example.test/{id}"));
        // A revoked URL stops resolving, which is what makes a worker started
        // from it fail with the reason the page expects.
        assert!(host.revoke_object_url(&url));
        assert_eq!(host.blob_bytes(&id), None);
        assert_eq!(
            host.start(&url, "", false),
            Err(format!("{url} is not a live blob URL"))
        );
        // Revoking something that is not an object URL is a no-op, not an error.
        assert!(!host.revoke_object_url("https://example.test/other"));
    }

    #[test]
    fn a_worker_needs_a_document_and_a_classic_script() {
        let host = WorkerHost::default();
        assert_eq!(
            host.start("worker.js", "", false),
            Err("workers are unavailable before a document is attached".to_owned())
        );
        // With a document, a module worker is refused by name (it would need a
        // module graph in its own realm).
        host.attach(WorkerAttachment {
            base_url: "https://example.test/main.html".to_owned(),
            private: crate::private_addr::PrivateAddressGuard::default(),
            fetch: Arc::new(|_| Ok(String::new())),
            jar: Arc::new(Mutex::new(ironsurf_storage::CookieJar::new())),
            net_events: Arc::new(Mutex::new(Vec::new())),
            user_agent: Arc::new(Mutex::new("IronSurf-test".to_owned())),
            wake: Box::new(|| {}),
        });
        let error = host.start("worker.js", "", true).expect_err("module workers fail");
        assert!(
            error.contains("module workers are not supported"),
            "{error}"
        );
    }

    #[test]
    fn a_worker_script_runs_answers_and_terminates() {
        let worker_script = r#"
                // The digest native answers with the 32 raw bytes as a latin1
                // string; hex here so the test asserts the real value.
                function hexOf(text) {
                  var out = "";
                  for (var i = 0; i < text.length; i++) {
                    var v = text.charCodeAt(i) & 0xFF;
                    out += (v < 16 ? "0" : "") + v.toString(16);
                  }
                  return out;
                }
                importScripts("/child.js");
                self.onmessage = function (event) {
                  var n = (event.data && event.data.n) || 0;
                  postMessage({
                    answered: n + 1,
                    child: CHILD_LOADED === true,
                    href: location.href,
                    origin: location.origin,
                    sha: hexOf(__ironsurfSha256("abc", 0)),
                    random: crypto.getRandomValues(new Uint8Array(4)).length,
                    uuid: /^[0-9a-f-]{36}$/.test(crypto.randomUUID()),
                    name: name
                  });
                  setTimeout(function () { postMessage("ticked"); }, 10);
                  fetch("/data.json").then(function (response) {
                    return response.text();
                  }).then(function (text) {
                    postMessage("body:" + text);
                  });
                };
                postMessage("ready");
        "#;
        let server = ToyServer::start(&[
            ("/worker.js", worker_script),
            ("/child.js", "var CHILD_LOADED = true;\n"),
            ("/data.json", "{\"hello\":true}"),
        ]);
        let host = WorkerHost::default();
        let wakes = attach_document(&host, &server);

        let id = host
            .start("/worker.js", "sidekick", false)
            .expect("worker starts");
        assert_eq!(host.worker_count(), 1);
        assert_eq!(
            host.base_url().as_deref(),
            Some(server.document_url().as_str())
        );

        let mut inbox = Inbox::new(&host);
        // The script ran on its own thread: its top-level `postMessage` arrives.
        let ready = inbox.wait_for(|message| message.payload == "\"ready\"");
        assert_eq!(ready.worker, id);

        host.post_to(id, serde_json::json!({ "n": 41 }).to_string());
        let answered = inbox.wait_for(|message| message.payload.contains("\"answered\":42"));
        assert_eq!(answered.worker, id);
        let answer = json(answered.payload.as_str());
        assert_eq!(answer["answered"], 42);
        assert_eq!(answer["child"], true, "importScripts ran first");
        assert_eq!(
            answer["href"],
            format!("http://127.0.0.1:{}/worker.js", server.port)
        );
        assert_eq!(
            answer["origin"],
            format!("http://127.0.0.1:{}", server.port)
        );
        assert_eq!(
            answer["sha"],
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(answer["random"], 4);
        assert_eq!(answer["uuid"], true);
        assert_eq!(answer["name"], "sidekick");

        // Timers fire in the idle worker, and `fetch` reached the toy server
        // through the tab's own blocking client.
        let ticked = inbox.wait_for(|message| message.payload == "\"ticked\"");
        assert_eq!(ticked.worker, id);
        // The payload is JSON text, so the fetched body arrives escaped inside it;
        // matching the prefix and the two words is enough to prove the reply
        // travelled the worker's own fetch path.
        let fetched = inbox.wait_for(|message| message.payload.starts_with("\"body:"));
        assert_eq!(fetched.worker, id);
        let body = fetched.payload;
        assert!(body.contains("hello") && body.contains("true"), "{body}");
        assert!(*wakes.lock().expect("wake counter") > 0);

        // `terminate` stops the thread: the id addresses nothing afterwards, the
        // registry is empty, and a dead worker posts nothing more.
        host.take_messages();
        assert!(host.terminate(id));
        assert_eq!(host.worker_count(), 0);
        assert!(!host.post_to(id, "null".to_owned()));
        std::thread::sleep(Duration::from_millis(40));
        assert!(host.take_messages().is_empty());
    }

    #[test]
    fn an_uncaught_throw_in_a_worker_is_reported_once() {
        let server = ToyServer::start(&[(
            "/boom.js",
            "postMessage('before');\nnull.boom;\npostMessage('after');\n",
        )]);
        let host = WorkerHost::default();
        attach_document(&host, &server);
        let id = host.start("/boom.js", "", false).expect("worker starts");
        let mut inbox = Inbox::new(&host);
        let reported = inbox.wait_for(|message| message.kind == "error");
        assert_eq!(reported.worker, id);
        let reported = reported.payload.as_str();
        assert!(
            reported.contains("null") && reported.contains("boom"),
            "{reported}"
        );
        // The worker is stopped by the throw, like the spec's "worker that
        // never runs": a message posted after the throw never arrives.
        std::thread::sleep(Duration::from_millis(40));
        inbox.seen.extend(host.take_messages());
        assert!(
            !inbox
                .seen()
                .iter()
                .any(|message| message.payload == "\"after\""),
            "{:?}",
            inbox.seen()
        );
    }

    #[test]
    fn a_worker_outliving_its_document_is_closed_not_leaked() {
        let server = ToyServer::start(&[(
            "/forever.js",
            "setInterval(function () { postMessage('beat'); }, 5);",
        )]);
        let host = WorkerHost::default();
        attach_document(&host, &server);
        let id = host.start("/forever.js", "", false).expect("worker starts");
        Inbox::new(&host).wait_for(|message| message.payload == "\"beat\"");
        host.close_all();
        assert_eq!(host.worker_count(), 0);
        // Close is idempotent (navigation teardown and drop both call it).
        host.close_all();
        assert!(!host.terminate(id));
        std::thread::sleep(Duration::from_millis(30));
        assert!(host.take_messages().is_empty());
    }

    /// Everything the tab loop does with a worker, in one realm: a page script's
    /// `new Worker(url)` starts a thread whose script comes from the document's
    /// script backend, its reply is queued on the bus, and the realm's delivery
    /// step hands it to the page's `onmessage` as a real object (this is the
    /// path a `<script>` in a page takes, minus the HTTP fixture and the
    /// process). Proves the page-facing glue, the worker realm and the bus are
    /// wired to each other, not just to this file's unit tests.
    #[test]
    fn a_page_realm_talks_to_a_worker_thread() {
        let server = ToyServer::start(&[(
            "/echo.js",
            "onmessage = function (e) { postMessage({ echo: e.data, threads: 2 }); };",
        )]);
        let host = std::rc::Rc::new(std::cell::RefCell::new(crate::js::HostState::new(
            server.document_url(),
            Some("Page"),
            Some(String::new()),
        )));
        attach_document(&host.borrow().workers, &server);
        let mut runtime = crate::js::JsRuntime::new(std::rc::Rc::clone(&host));
        runtime
            .eval(
                "var seen = [];
                 var worker = new Worker('/echo.js');
                 worker.onmessage = function (e) { seen.push(String(e.data.echo) + ':' + e.data.threads); };
                 worker.onerror = function (e) { seen.push('error:' + (e.message || '')); };
                 worker.postMessage('ping');
                 'started'",
            )
            .expect("the page script should run");
        let mut delivered = 0usize;
        for _ in 0..100 {
            delivered += runtime.deliver_worker_messages();
            if delivered > 0 {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(delivered > 0, "the worker's reply never reached the realm");
        let seen = runtime.eval("seen.join('|')").expect("read what the page saw");
        assert_eq!(
            seen.as_string().map(|text| text.to_std_string_escaped()),
            Some("ping:2".to_owned()),
            "the page's onmessage should get the posted object"
        );
    }
}
