//! MarkSync sync server: a Rust implementation of the xBrowserSync REST API.

pub mod config;
pub mod error;
pub mod http;
pub mod location;
pub mod service;
pub mod store;
pub mod throttle;
pub mod version;

use std::sync::Arc;

use anyhow::Result;

use crate::config::Config;
use crate::http::App;
use crate::service::Service;
use crate::store::Store;

/// Connects to storage and builds the application state.
pub async fn build_app(config: Config) -> Result<Arc<App>> {
    let store = Store::connect(&config).await?;
    Ok(Arc::new(App::new(Service::new(config, store))))
}
