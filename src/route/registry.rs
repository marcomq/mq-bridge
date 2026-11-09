//  mq-bridge
//  © Copyright 2025, by Marco Mengelkoch
//  Licensed under MIT OR Apache-2.0, see LICENSE file for more details
//  git clone https://github.com/marcomq/mq-bridge

//! Process-wide state: deployed routes, named endpoints (`ref`) and display names.

use super::*;

pub(super) static ROUTE_REGISTRY: OnceLock<RwLock<HashMap<String, ActiveRoute>>> = OnceLock::new();

pub(super) static ENDPOINT_REF_REGISTRY: OnceLock<RwLock<HashMap<String, Endpoint>>> =
    OnceLock::new();

pub(super) fn recover_read_lock<'a, T>(lock: &'a RwLock<T>, name: &str) -> RwLockReadGuard<'a, T> {
    lock.read().unwrap_or_else(|poisoned| {
        warn!(lock = name, "Recovering from poisoned read lock");
        poisoned.into_inner()
    })
}

thread_local! {
    static ENDPOINT_REF_OVERLAY: std::cell::RefCell<Option<EndpointOverlay>> =
        const { std::cell::RefCell::new(None) };
}

pub(super) static ROUTE_DISPLAY_NAMES: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

pub(super) type EndpointOverlay = HashMap<String, Option<Endpoint>>;

pub(super) struct ActiveRoute {
    route: Route,
    handle: RouteHandle,
}

pub(super) fn recover_write_lock<'a, T>(
    lock: &'a RwLock<T>,
    name: &str,
) -> RwLockWriteGuard<'a, T> {
    lock.write().unwrap_or_else(|poisoned| {
        warn!(lock = name, "Recovering from poisoned write lock");
        poisoned.into_inner()
    })
}

/// Registers a named endpoint that can be referenced by other endpoints using `ref: "name"`.
/// This will overwrite any existing endpoint with the same name.
pub fn register_endpoint(name: &str, endpoint: Endpoint) {
    let registry = ENDPOINT_REF_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let mut writer = recover_write_lock(registry, "endpoint_ref_registry");
    if writer.insert(name.to_string(), endpoint).is_some() {
        debug!("Overwriting a registered endpoint named '{}'", name);
    }
}

/// Runs `f` with `overrides` layered over the registered endpoints on the current thread only,
/// so a configuration can be checked against the refs it would register without publishing
/// them. `None` hides a registered name. Other threads keep seeing the registry unchanged.
pub fn with_endpoint_overrides<R>(overrides: EndpointOverlay, f: impl FnOnce() -> R) -> R {
    struct Restore(Option<EndpointOverlay>);
    impl Drop for Restore {
        fn drop(&mut self) {
            let previous = self.0.take();
            ENDPOINT_REF_OVERLAY.with(|overlay| *overlay.borrow_mut() = previous);
        }
    }
    let _restore = Restore(ENDPOINT_REF_OVERLAY.with(|overlay| overlay.replace(Some(overrides))));
    f()
}

/// Retrieves a registered endpoint by name.
pub fn get_endpoint(name: &str) -> Option<Endpoint> {
    let overridden = ENDPOINT_REF_OVERLAY.with(|overlay| {
        overlay
            .borrow()
            .as_ref()
            .and_then(|overlay| overlay.get(name).cloned())
    });
    if let Some(endpoint) = overridden {
        return endpoint;
    }
    let registry = ENDPOINT_REF_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let reader = recover_read_lock(registry, "endpoint_ref_registry");
    reader.get(name).cloned()
}

/// Removes a named endpoint, so `ref: "name"` no longer resolves. Returns the removed endpoint.
pub fn unregister_endpoint(name: &str) -> Option<Endpoint> {
    let mut writer = recover_write_lock(ENDPOINT_REF_REGISTRY.get()?, "endpoint_ref_registry");
    writer.remove(name)
}

/// Sets the name `route` is shown under in log lines, error text and DLQ metadata.
/// `route` stays the key: the registry, metric labels and default store tables keep it.
pub fn set_route_display_name(route: &str, display: &str) {
    let registry = ROUTE_DISPLAY_NAMES.get_or_init(|| RwLock::new(HashMap::new()));
    recover_write_lock(registry, "route_display_names")
        .insert(route.to_string(), display.to_string());
}

pub(super) fn display_name_of(route: &str) -> Option<String> {
    let reader = recover_read_lock(ROUTE_DISPLAY_NAMES.get()?, "route_display_names");
    reader.get(route).cloned()
}

/// The name to print for `route`: its display name, or `route` itself.
pub(crate) fn route_display_name(route: &str) -> String {
    display_name_of(route).unwrap_or_else(|| route.to_string())
}

/// Swaps the route key for its display name where text embeds it: `[route:…]` or quoted.
pub(super) fn show_route_name(text: String, name: &str, shown: &str) -> String {
    if name == shown {
        return text;
    }
    [("[route:", "]"), ("'", "'"), ("`", "`")]
        .iter()
        .fold(text, |text, (open, close)| {
            text.replace(
                &format!("{open}{name}{close}"),
                &format!("{open}{shown}{close}"),
            )
        })
}

/// [`show_route_name`] for an error. Left as it is, type included, without a display name.
pub(super) fn show_route_error(error: anyhow::Error, name: &str, shown: &str) -> anyhow::Error {
    if name == shown {
        return error;
    }
    anyhow::anyhow!(show_route_name(format!("{error:#}"), name, shown))
}

pub fn get_route(name: &str) -> Option<Route> {
    Route::get(name)
}

pub fn list_routes() -> Vec<String> {
    Route::list()
}

/// Returns why a deployed route terminated, or `None` while it is still running
/// (or if nothing is deployed under `name`).
///
/// [`Route::deploy`] keeps its [`RouteHandle`] private, so this is how a caller
/// that deployed by name — the language bindings, a supervisor — learns that a
/// drain-then-exit route finished on its own rather than waiting on a `stop()`
/// that never comes.
pub fn route_outcome(name: &str) -> Option<RouteOutcome> {
    let registry = ROUTE_REGISTRY.get()?;
    let map = recover_read_lock(registry, "route_registry");
    map.get(name)?.handle.outcome()
}

/// Returns the connection health of a deployed route, or `None` if nothing is
/// deployed under `name`. Pairs with [`route_outcome`]: after a
/// [`RouteOutcome::Failed`], `error` holds the cause.
pub fn route_status(name: &str) -> Option<EndpointStatus> {
    let registry = ROUTE_REGISTRY.get()?;
    let map = recover_read_lock(registry, "route_registry");
    Some(map.get(name)?.handle.status())
}

pub async fn stop_route(name: &str) -> bool {
    Route::stop(name).await
}

impl Route {
    /// Creates a new route with default concurrency (1) and batch size (512).
    ///
    /// # Arguments
    /// * `input` - The input/source endpoint for the route
    /// * `output` - The output/sink endpoint for the route
    pub fn new(input: Endpoint, output: Endpoint) -> Self {
        Self {
            input,
            output,
            ..Default::default()
        }
    }

    /// Retrieves a registered (and running) route by name.
    pub fn get(name: &str) -> Option<Self> {
        let registry = ROUTE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let map = recover_read_lock(registry, "route_registry");
        map.get(name).map(|active| active.route.clone())
    }

    /// Returns a list of all registered route names.
    pub fn list() -> Vec<String> {
        let registry = ROUTE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let map = recover_read_lock(registry, "route_registry");
        map.keys().cloned().collect()
    }

    /// Returns true if the input is of type ref (and the output isn't)
    pub fn is_ref(&self) -> bool {
        matches!(self.input.endpoint_type, EndpointType::Ref(_))
            && !matches!(self.output.endpoint_type, EndpointType::Ref(_))
    }

    /// Registers the route's output endpoint under the given name.
    /// This allows other routes to reference this output using `ref: "name"`.
    pub fn register_output_endpoint(&self, name: Option<&str>) -> Result<(), anyhow::Error> {
        match name {
            Some(name) => {
                register_endpoint(name, self.output.clone());
            }
            None => {
                if let EndpointType::Ref(name) = &self.input.endpoint_type {
                    register_endpoint(name, self.output.clone());
                } else {
                    return Err(anyhow::anyhow!(
                        "No name and input is not a reference endpoint"
                    ));
                }
            }
        };
        Ok(())
    }

    /// Registers the route and starts it.
    /// If a route with the same name is already running, it will be stopped first.
    ///    
    /// # Examples
    /// ```
    /// use mq_bridge::{Route, models::Endpoint};
    ///
    /// let route = Route::new(Endpoint::new_memory("in", 10), Endpoint::new_memory("out", 10));
    /// # tokio::runtime::Runtime::new().unwrap().block_on(async {
    /// route.deploy("global_route").await.unwrap();
    /// assert!(Route::get("global_route").is_some());
    /// # });
    /// ```
    pub async fn deploy(&self, name: &str) -> anyhow::Result<()> {
        Self::stop(name).await;

        let handle = self.run(name).await?;
        let active = ActiveRoute {
            route: self.clone(),
            handle,
        };

        let registry = ROUTE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let replaced = {
            let mut map = recover_write_lock(registry, "route_registry");
            map.insert(name.to_string(), active)
        };
        // A concurrent deploy of the same name may have registered in between.
        if let Some(replaced) = replaced {
            Self::stop_active(replaced).await;
        }
        Ok(())
    }

    /// Stops a running route by name and removes it from the registry.
    /// Waits up to 5 seconds for the route task to join; if the timeout elapses
    /// the task is aborted and the implementation awaits the aborted handle to
    /// ensure the background task has fully terminated before returning.
    pub async fn stop(name: &str) -> bool {
        let registry = ROUTE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
        let active_opt = {
            let mut map = recover_write_lock(registry, "route_registry");
            map.remove(name)
        };

        if let Some(active) = active_opt {
            Self::stop_active(active).await;
            true
        } else {
            false
        }
    }

    pub(super) async fn stop_active(active: ActiveRoute) {
        // Move the handle out so we can operate on its internals.
        let handle = active.handle;

        // Signal the route to stop and close the shutdown channel.
        let _ = handle.shutdown_tx.send(()).await;
        handle.shutdown_tx.close();

        // Extract the JoinHandle so we can monitor and, if needed, abort it.
        let mut join_handle = handle.handle;
        tokio::select! {
            res = &mut join_handle => {
                // The task finished naturally within the 5s window
                let _ = res;
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                // The 5s timer finished first - abort the task to ensure it doesn't linger.
                tracing::warn!(
                    "Route did not stop within 5s; aborting it. Disconnect hooks are skipped and in-flight messages are redelivered."
                );
                join_handle.abort();
                // Await the handle one last time to ensure the task has fully shut down.
                let _ = join_handle.await;
            }
        }
    }
}
