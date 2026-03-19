use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::State,
    http::{HeaderValue, StatusCode, header::CONTENT_TYPE},
    response::{IntoResponse, Response},
    routing::get,
};
use http_api::HttpServerConfig;
use metrics::metrics_module;
use tokio::sync::{Mutex, Notify, mpsc};
use tracing::{info, warn};

use crate::environment::EventSource;

#[derive(Debug, Clone)]
pub enum HttpRequest {
    Health,
    FinalizedState,
    JustifiedCheckpoint,
}

#[derive(Debug, Clone)]
pub enum HttpResponse {
    HealthOk(Vec<u8>),
    FinalizedStateOk(Vec<u8>),
    JustifiedCheckpointOk(Vec<u8>),
    FinalizedStateNotFound,
    ServiceUnavailable,
    InternalServerError,
}

#[derive(Debug, Clone)]
pub enum HttpEvent {
    RequestReceived {
        request_id: u64,
        request: HttpRequest,
    },
}

#[derive(Debug, Clone)]
pub enum HttpEffect {
    Respond {
        request_id: u64,
        response: HttpResponse,
    },
}

struct PendingResponse {
    response: Option<HttpResponse>,
    notify: Arc<Notify>,
}

#[derive(Clone)]
struct HttpBridgeState {
    next_request_id: Arc<AtomicU64>,
    pending: Arc<Mutex<HashMap<u64, PendingResponse>>>,
    event_tx: mpsc::UnboundedSender<HttpEvent>,
}

impl HttpBridgeState {
    async fn dispatch(&self, request: HttpRequest) -> Response {
        let request_id = self.next_request_id.fetch_add(1, Ordering::Relaxed);
        let notify = Arc::new(Notify::new());

        {
            let mut pending = self.pending.lock().await;
            pending.insert(
                request_id,
                PendingResponse {
                    response: None,
                    notify: notify.clone(),
                },
            );
        }

        if self
            .event_tx
            .send(HttpEvent::RequestReceived {
                request_id,
                request,
            })
            .is_err()
        {
            let mut pending = self.pending.lock().await;
            pending.remove(&request_id);
            return HttpResponse::ServiceUnavailable.into_response();
        }

        loop {
            let response = {
                let mut pending = self.pending.lock().await;
                if let Some(entry) = pending.get_mut(&request_id) {
                    if let Some(response) = entry.response.take() {
                        pending.remove(&request_id);
                        Some(response)
                    } else {
                        None
                    }
                } else {
                    Some(HttpResponse::ServiceUnavailable)
                }
            };

            if let Some(response) = response {
                return response.into_response();
            }

            notify.notified().await;
        }
    }
}

pub struct HttpEventSource {
    config: HttpServerConfig,
}

impl HttpEventSource {
    #[must_use]
    pub fn new(config: HttpServerConfig) -> Self {
        Self { config }
    }
}

impl EventSource for HttpEventSource {
    type Event = HttpEvent;
    type Effect = HttpEffect;

    async fn run(
        &mut self,
        event_tx: mpsc::UnboundedSender<Self::Event>,
        mut effect_rx: mpsc::UnboundedReceiver<Self::Effect>,
    ) -> Result<()> {
        let bridge_state = HttpBridgeState {
            next_request_id: Arc::new(AtomicU64::new(1)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            event_tx,
        };

        let mut router = Router::new()
            .route("/lean/v0/health", get(get_health))
            .route("/lean/v0/states/finalized", get(get_finalized_state))
            .route(
                "/lean/v0/checkpoints/justified",
                get(get_justified_checkpoint),
            )
            .with_state(bridge_state.clone());

        if self.config.metrics_enabled() {
            router = router.merge(metrics_module(self.config.metrics().clone()));
        }

        let listener = tokio::net::TcpListener::bind(self.config.address())
            .await
            .context("failed to start http server")?;

        let service = router.into_make_service_with_connect_info::<std::net::SocketAddr>();

        info!("HTTP server listening on {}", self.config.address());

        tokio::select! {
            serve_result = axum::serve(listener, service) => {
                serve_result.context("http server stopped unexpectedly")
            }
            _ = async {
                while let Some(effect) = effect_rx.recv().await {
                    match effect {
                        HttpEffect::Respond { request_id, response } => {
                            let notify = {
                                let mut pending = bridge_state.pending.lock().await;

                                match pending.get_mut(&request_id) {
                                    Some(entry) => {
                                        entry.response = Some(response);
                                        Some(entry.notify.clone())
                                    }
                                    None => {
                                        warn!(request_id, "received http response for unknown request id");
                                        None
                                    }
                                }
                            };

                            if let Some(notify) = notify {
                                notify.notify_waiters();
                            }
                        }
                    }
                }
            } => Ok(())
        }
    }
}

impl IntoResponse for HttpResponse {
    fn into_response(self) -> Response {
        match self {
            HttpResponse::HealthOk(body) | HttpResponse::JustifiedCheckpointOk(body) => {
                let mut response = (StatusCode::OK, body).into_response();
                response
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                response
            }
            HttpResponse::FinalizedStateOk(body) => {
                let mut response = (StatusCode::OK, body).into_response();
                response.headers_mut().insert(
                    CONTENT_TYPE,
                    HeaderValue::from_static("application/octet-stream"),
                );
                response
            }
            HttpResponse::FinalizedStateNotFound => {
                (StatusCode::NOT_FOUND, "Finalized state not available").into_response()
            }
            HttpResponse::ServiceUnavailable => {
                (StatusCode::SERVICE_UNAVAILABLE, "Store not initialized").into_response()
            }
            HttpResponse::InternalServerError => {
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal server error").into_response()
            }
        }
    }
}

async fn get_health(State(state): State<HttpBridgeState>) -> Response {
    state.dispatch(HttpRequest::Health).await
}

async fn get_finalized_state(State(state): State<HttpBridgeState>) -> Response {
    state.dispatch(HttpRequest::FinalizedState).await
}

async fn get_justified_checkpoint(State(state): State<HttpBridgeState>) -> Response {
    state.dispatch(HttpRequest::JustifiedCheckpoint).await
}
