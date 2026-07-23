use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Json, Response};

use super::index;
use super::lodjit;
use super::resolver;
use super::serve;
use super::state::AppState;

mod dispatch;
mod entities;
mod fresh;
mod jit;
mod status;
mod upstream;

use dispatch::*;
#[cfg(test)]
use entities::*;
use fresh::*;
use jit::*;
#[cfg(test)]
use status::*;
use upstream::*;

pub use dispatch::dispatch;
pub use entities::{post_entities_active, post_entities_versions};
pub use status::{get_build_progress, health, livez, metrics, ping, readyz};

#[cfg(test)]
mod tests;
