use axum::extract::State;

use crate::Metrics;

/// `GET /metrics`
async fn get_metrics(State(metrics): State<Arc<Metrics>>) -> Result<String> {}
