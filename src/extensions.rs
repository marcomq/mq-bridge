use crate::traits::{CustomEndpointFactory, CustomMiddlewareFactory};
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

static ENDPOINT_REGISTRY: OnceLock<RwLock<HashMap<String, Arc<dyn CustomEndpointFactory>>>> =
    OnceLock::new();
static MIDDLEWARE_REGISTRY: OnceLock<RwLock<HashMap<String, Arc<dyn CustomMiddlewareFactory>>>> =
    OnceLock::new();

pub fn register_endpoint_factory(name: &str, factory: Arc<dyn CustomEndpointFactory>) {
    let registry = ENDPOINT_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let mut map = registry.write().expect("Endpoint registry lock poisoned");
    map.insert(name.to_string(), factory);
}

pub fn get_endpoint_factory(name: &str) -> Option<Arc<dyn CustomEndpointFactory>> {
    let registry = ENDPOINT_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let map = registry.read().expect("Endpoint registry lock poisoned");
    map.get(name).cloned()
}

pub fn register_middleware_factory(name: &str, factory: Arc<dyn CustomMiddlewareFactory>) {
    let registry = MIDDLEWARE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let mut map = registry.write().expect("Middleware registry lock poisoned");
    map.insert(name.to_string(), factory);
}

pub fn get_middleware_factory(name: &str) -> Option<Arc<dyn CustomMiddlewareFactory>> {
    let registry = MIDDLEWARE_REGISTRY.get_or_init(|| RwLock::new(HashMap::new()));
    let map = registry.read().expect("Middleware registry lock poisoned");
    map.get(name).cloned()
}
