use containers::{Checkpoint, State};
use serde::Serialize;
use ssz::SszWrite as _;

use crate::{
    HttpResponse,
    chain::ChainMessage,
    environment::{Effect, Event, Service, ServiceInput, ServiceOutput},
    http::{HttpEvent, HttpRequest, event_source::HttpEffect},
};

#[derive(Debug, Clone)]
pub enum HttpMessage {
    FinalizedState {
        request_id: u64,
        state: Option<State>,
    },
    JustifiedCheckpoint {
        request_id: u64,
        checkpoint: Checkpoint,
    },
}

#[derive(Serialize)]
struct HealthResponse {
    status: &'static str,
    service: &'static str,
}

#[derive(Serialize)]
struct JustifiedCheckpointResponse {
    slot: u64,
    root: String,
}

#[derive(Clone)]
pub struct HttpService;

impl HttpService {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Service for HttpService {
    type Message = HttpMessage;

    fn handle_input(&mut self, input: ServiceInput<Self::Message>) -> ServiceOutput {
        match input {
            ServiceInput::Event(Event::Http(HttpEvent::RequestReceived {
                request_id,
                request,
            })) => match request {
                HttpRequest::Health => {
                    let response = serde_json::to_vec(&HealthResponse {
                        status: "healthy",
                        service: "lean-rpc-api",
                    })
                    .map_or(HttpResponse::InternalServerError, HttpResponse::HealthOk);

                    ServiceOutput::none().with_effect(Effect::Http(HttpEffect::Respond {
                        request_id,
                        response,
                    }))
                }
                HttpRequest::FinalizedState => {
                    ServiceOutput::chain_message(ChainMessage::GetFinalizedState { request_id })
                }
                HttpRequest::JustifiedCheckpoint => {
                    ServiceOutput::chain_message(ChainMessage::GetJustifiedCheckpoint {
                        request_id,
                    })
                }
            },
            ServiceInput::Message(HttpMessage::FinalizedState { request_id, state }) => {
                let response = match state {
                    Some(state) => state.to_ssz().map_or(
                        HttpResponse::InternalServerError,
                        HttpResponse::FinalizedStateOk,
                    ),
                    None => HttpResponse::FinalizedStateNotFound,
                };

                ServiceOutput::none().with_effect(Effect::Http(HttpEffect::Respond {
                    request_id,
                    response,
                }))
            }
            ServiceInput::Message(HttpMessage::JustifiedCheckpoint {
                request_id,
                checkpoint,
            }) => {
                let response = serde_json::to_vec(&JustifiedCheckpointResponse {
                    slot: checkpoint.slot.0,
                    root: format!("0x{:x}", checkpoint.root),
                })
                .map_or(
                    HttpResponse::InternalServerError,
                    HttpResponse::JustifiedCheckpointOk,
                );

                ServiceOutput::none().with_effect(Effect::Http(HttpEffect::Respond {
                    request_id,
                    response,
                }))
            }
            ServiceInput::Event(_) => ServiceOutput::none(),
        }
    }
}
