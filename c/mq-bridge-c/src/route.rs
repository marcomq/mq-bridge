use std::collections::HashSet;
use std::ffi::{c_char, c_void};
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use mq_bridge_bindings_common as common;
use mqb::support::plugin_abi::{MqbStatus, MQB_ERR_CONNECTION, MQB_ERR_RETRYABLE, MQB_OK};
use mqb::traits::Handler;
use mqb::type_handler::TypeHandler;
use mqb::{CanonicalMessage, Handled, HandlerError, Route, RouteOutcome};
use tokio::runtime::Runtime;
use tokio::sync::oneshot;

use crate::message::mqb_message_t;
use crate::{guard, guard_new, handle, optional_name, parse_config, text};

/// How often a running route is checked for having ended on its own.
const ROUTE_END_POLL_INTERVAL: Duration = Duration::from_millis(25);

/// Handles one message of a route. `message` is valid for the call only. Leave
/// `*out` null to acknowledge, or set it to a new message to publish that instead
/// (the library frees it). Return `MQB_OK`, `MQB_ERR_RETRYABLE` to have the
/// message redelivered, or any other status to drop it as failed.
///
/// Called from the library's worker threads, possibly several at once.
pub type mqb_handler_fn = Option<
    unsafe extern "C" fn(
        message: *const mqb_message_t,
        out: *mut *mut mqb_message_t,
        user_data: *mut c_void,
    ) -> MqbStatus,
>;

struct CHandler {
    label: String,
    callback: unsafe extern "C" fn(
        *const mqb_message_t,
        *mut *mut mqb_message_t,
        *mut c_void,
    ) -> MqbStatus,
    user_data: usize,
}

#[async_trait]
impl Handler for CHandler {
    async fn handle(&self, msg: CanonicalMessage) -> Result<Handled, HandlerError> {
        let input = mqb_message_t::wrap(msg);
        // C code may block; keep the runtime's other tasks moving meanwhile.
        let (status, reply) = tokio::task::block_in_place(|| {
            let mut out = std::ptr::null_mut();
            let status =
                unsafe { (self.callback)(&input, &mut out, self.user_data as *mut c_void) };
            let reply = (!out.is_null()).then(|| unsafe { Box::from_raw(out) });
            (status, reply)
        });
        let failed = || anyhow!("C handler '{}' returned status {status}", self.label);
        match status {
            MQB_OK => Ok(reply.map_or(Handled::Ack, |reply| Handled::Publish(reply.inner))),
            MQB_ERR_RETRYABLE => Err(HandlerError::Retryable(failed())),
            MQB_ERR_CONNECTION => Err(HandlerError::Connection(failed())),
            _ => Err(HandlerError::NonRetryable(failed())),
        }
    }
}

#[derive(Default)]
struct RunState {
    running: bool,
    stop_tx: Option<oneshot::Sender<()>>,
    join_handle: Option<thread::JoinHandle<()>>,
    // Why a started route ended on a permanent failure; taken by join.
    failure: Option<String>,
}

/// One route: an input endpoint, optional handlers, an output endpoint.
pub struct mqb_route_t {
    runtime: Arc<Runtime>,
    route: Mutex<Route>,
    name: String,
    run_state: Arc<Mutex<RunState>>,
}

fn build(route: Route, name: Option<&str>) -> anyhow::Result<mqb_route_t> {
    Ok(mqb_route_t {
        runtime: Arc::new(common::build_runtime()?),
        route: Mutex::new(route),
        name: name.map_or_else(common::default_route_name, str::to_string),
        run_state: Arc::new(Mutex::new(RunState::default())),
    })
}

fn active_route_names() -> &'static Mutex<HashSet<String>> {
    static ACTIVE: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    ACTIVE.get_or_init(|| Mutex::new(HashSet::new()))
}

fn finish_run(run_state: &Mutex<RunState>, name: &str) {
    if let Ok(mut active) = active_route_names().lock() {
        active.remove(name);
    }
    if let Ok(mut state) = run_state.lock() {
        state.running = false;
        state.stop_tx = None;
    }
}

/// Waits for a stop request, or for the route to end on its own (a drained source
/// under `exit_on_empty`, a permanent failure). `None` means it was asked to stop.
async fn wait_for_stop_or_end(name: &str, stop_rx: oneshot::Receiver<()>) -> Option<RouteOutcome> {
    tokio::pin!(stop_rx);
    loop {
        tokio::select! {
            _ = &mut stop_rx => return None,
            _ = mqb::shutdown::shutdown_requested() => return None,
            _ = tokio::time::sleep(ROUTE_END_POLL_INTERVAL) => {
                if let Some(outcome) = mqb::route_outcome(name) {
                    return Some(outcome);
                }
            }
        }
    }
}

impl mqb_route_t {
    fn lock_state(&self) -> anyhow::Result<std::sync::MutexGuard<'_, RunState>> {
        self.run_state
            .lock()
            .map_err(|_| anyhow!("route state lock poisoned"))
    }

    fn lock_route(&self) -> anyhow::Result<std::sync::MutexGuard<'_, Route>> {
        if self.lock_state()?.running {
            return Err(anyhow!(
                "route handlers cannot be modified while the route is running"
            ));
        }
        self.route
            .lock()
            .map_err(|_| anyhow!("route lock poisoned"))
    }

    fn handler(
        &self,
        label: String,
        callback: mqb_handler_fn,
        user_data: *mut c_void,
    ) -> anyhow::Result<CHandler> {
        Ok(CHandler {
            label,
            callback: callback.ok_or_else(|| anyhow!("handler is null"))?,
            user_data: user_data as usize,
        })
    }

    fn begin_run(&self) -> anyhow::Result<oneshot::Receiver<()>> {
        let mut state = self.lock_state()?;
        if state.running {
            return Err(anyhow!("route is already running"));
        }
        let mut active = active_route_names()
            .lock()
            .map_err(|_| anyhow!("active route registry lock poisoned"))?;
        if active.contains(&self.name) || Route::get(&self.name).is_some() {
            return Err(anyhow!("a route named '{}' is already running", self.name));
        }
        active.insert(self.name.clone());
        let (stop_tx, stop_rx) = oneshot::channel();
        state.running = true;
        state.stop_tx = Some(stop_tx);
        state.failure = None;
        Ok(stop_rx)
    }

    fn start(&self) -> anyhow::Result<()> {
        let route = self
            .route
            .lock()
            .map_err(|_| anyhow!("route lock poisoned"))?
            .clone();
        let stop_rx = self.begin_run()?;
        let name = self.name.clone();
        if let Err(err) = self.runtime.block_on(route.deploy(&name)) {
            finish_run(&self.run_state, &name);
            return Err(err);
        }

        let runtime = Arc::clone(&self.runtime);
        let run_state = Arc::clone(&self.run_state);
        let thread_name = name.clone();
        let spawned = thread::Builder::new()
            .name(format!("mqb-route-{name}"))
            .spawn(move || {
                let name = thread_name;
                let outcome = runtime.block_on(async {
                    let outcome = wait_for_stop_or_end(&name, stop_rx).await;
                    let failure = (outcome == Some(RouteOutcome::Failed)).then(|| {
                        mqb::route_status(&name)
                            .and_then(|status| status.error)
                            .unwrap_or_else(|| "permanent error".to_string())
                    });
                    Route::stop(&name).await;
                    failure
                });
                if let (Some(cause), Ok(mut state)) = (outcome, run_state.lock()) {
                    state.failure = Some(format!("route '{name}' failed: {cause}"));
                }
                finish_run(&run_state, &name);
            });
        match spawned {
            Ok(join_handle) => {
                self.lock_state()?.join_handle = Some(join_handle);
                Ok(())
            }
            Err(err) => {
                // Deployed but unwatched: stop it so the name can be started again.
                self.runtime.block_on(Route::stop(&name));
                finish_run(&self.run_state, &name);
                Err(err.into())
            }
        }
    }

    fn stop(&self) -> anyhow::Result<()> {
        if let Some(stop_tx) = self.lock_state()?.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        Ok(())
    }

    fn join(&self) -> anyhow::Result<()> {
        let join_handle = self.lock_state()?.join_handle.take();
        if let Some(join_handle) = join_handle {
            join_handle
                .join()
                .map_err(|_| anyhow!("route background thread panicked"))?;
        }
        match self.lock_state()?.failure.take() {
            Some(failure) => Err(anyhow!(failure)),
            None => Ok(()),
        }
    }
}

/// Builds a route from a YAML or JSON config file: a `routes:` document, a
/// `{name: route}` map, or a single route body (`name` null or `""`). Null on error.
#[no_mangle]
pub unsafe extern "C" fn mqb_route_from_file(
    path: *const c_char,
    name: *const c_char,
) -> *mut mqb_route_t {
    guard_new(|| {
        let path = unsafe { text(path, "path") }?;
        let name = unsafe { optional_name(name) }?;
        build(common::load_named_route(Path::new(path), name)?, name)
    })
}

/// Like `mqb_route_from_file`, from YAML or JSON text.
#[no_mangle]
pub unsafe extern "C" fn mqb_route_from_str(
    config: *const c_char,
    name: *const c_char,
) -> *mut mqb_route_t {
    guard_new(|| {
        let value = parse_config(unsafe { text(config, "config") }?)?;
        let name = unsafe { optional_name(name) }?;
        build(common::named_route_from_value(value, name)?, name)
    })
}

/// Runs every message through `handler` before the output. Set before `mqb_route_start`.
#[no_mangle]
pub unsafe extern "C" fn mqb_route_set_handler(
    route: *const mqb_route_t,
    handler: mqb_handler_fn,
    user_data: *mut c_void,
) -> MqbStatus {
    guard(|| {
        let route = unsafe { handle(route) }?;
        let handler = route.handler(route.name.clone(), handler, user_data)?;
        let mut inner = route.lock_route()?;
        *inner = inner.clone().with_handler(handler);
        Ok(())
    })
}

/// Runs messages whose `kind` metadata equals `kind` through `handler`. May be
/// called once per kind. Set before `mqb_route_start`.
#[no_mangle]
pub unsafe extern "C" fn mqb_route_add_handler(
    route: *const mqb_route_t,
    kind: *const c_char,
    handler: mqb_handler_fn,
    user_data: *mut c_void,
) -> MqbStatus {
    guard(|| {
        let route = unsafe { handle(route) }?;
        let kind = unsafe { text(kind, "kind") }?;
        let handler: Arc<dyn Handler> =
            Arc::new(route.handler(format!("{}:{kind}", route.name), handler, user_data)?);
        let mut inner = route.lock_route()?;
        let typed: Arc<dyn Handler> = match inner.output.handler.take() {
            Some(existing) => match existing.register_handler(kind, Arc::clone(&handler)) {
                Some(extended) => extended,
                None => Arc::new(
                    TypeHandler::new()
                        .with_fallback(existing)
                        .add_handler(kind, handler),
                ),
            },
            None => Arc::new(TypeHandler::new().add_handler(kind, handler)),
        };
        inner.output.handler = Some(typed);
        Ok(())
    })
}

/// Connects both endpoints and starts moving messages in the background.
#[no_mangle]
pub unsafe extern "C" fn mqb_route_start(route: *const mqb_route_t) -> MqbStatus {
    guard(|| unsafe { handle(route) }?.start())
}

/// Asks a running route to stop; `mqb_route_join` waits for it.
#[no_mangle]
pub unsafe extern "C" fn mqb_route_stop(route: *const mqb_route_t) -> MqbStatus {
    guard(|| unsafe { handle(route) }?.stop())
}

/// Blocks until the route has stopped, by `mqb_route_stop` or on its own (a
/// drained source under `exit_on_empty`). Fails if it ended on a permanent error.
#[no_mangle]
pub unsafe extern "C" fn mqb_route_join(route: *const mqb_route_t) -> MqbStatus {
    guard(|| unsafe { handle(route) }?.join())
}

/// Stops the route if it is running, waits for it, and frees it.
#[no_mangle]
pub unsafe extern "C" fn mqb_route_free(route: *mut mqb_route_t) {
    if !route.is_null() {
        let route = unsafe { Box::from_raw(route) };
        let _ = route.stop();
        let _ = route.join();
    }
}
