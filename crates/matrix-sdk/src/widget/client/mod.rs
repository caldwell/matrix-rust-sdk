//! Client widget API implementation.

#![warn(unreachable_pub)]

use std::{
    collections::HashMap,
    result::Result as StdResult,
    sync::{Arc, Mutex},
};

use async_channel::Sender;
use serde_json::{from_str as from_json, to_string as to_json};
use tokio::sync::oneshot;
use tracing::warn;
use uuid::Uuid;

use self::handler::{IncomingResponse, MessageHandler, OutgoingRequest};
pub(crate) use self::{
    handler::{Error, Result},
    matrix::Driver as MatrixDriver,
};
use super::{
    messages::{to_widget::Action as ToWidgetAction, Action, Header, Message},
    PermissionsProvider, Widget, WidgetSettings as WidgetInfo,
};

mod handler;
mod matrix;

/// Runs the client widget API handler for a given widget with a provided
/// `client`. Returns once the widget is disconnected or some terminal error
/// occurs.
pub(super) async fn run<T: PermissionsProvider>(
    client: MatrixDriver<T>,
    widget: Widget,
) -> Result<()> {
    // A map of outgoing requests that we are waiting a response for, i.e. a
    // requests that we sent to the widget and that we're expecting an answer
    // from.
    let state: PendingResponses = Arc::new(Mutex::new(HashMap::new()));

    // Create a message handler (handles incoming requests from the widget).
    let handler = {
        let widget = WidgetProxy::new(widget.settings, widget.comm.to, state.clone());
        MessageHandler::new(client, widget)
    };

    // Receives plain JSON string messages from a widget, parses it and passes
    // it to the handler.
    while let Ok(raw) = widget.comm.from.recv().await {
        match from_json::<Message>(&raw) {
            Ok(msg) => match msg.action {
                // This is an incoming request from a widget.
                Action::FromWidget(action) => {
                    handler.handle(msg.header, action).await?;
                }
                // This is a response from the widget to a request from the
                // client / driver (i.e. response to the outgoing request from
                // our perspective).
                Action::ToWidget(action) => {
                    let pending = state
                        .lock()
                        .expect("Pending mutex poisoned")
                        .remove(&msg.header.request_id);

                    if let Some(tx) = pending {
                        // It's ok if send fails here, it just means that the
                        // widget has disconnected.
                        let _ = tx.send(action);
                    }

                    // TODO: We don't seem to have an error response for the
                    // unexpected **responses** to our requests. Only for errors
                    // that occur during the processing of the **incoming
                    // requests**, not the **outgoing requests** (requests sent
                    // by us **to** the widget). Clarify that later.
                }
            },
            Err(e) => {
                // TODO: We need to construct a message to inform the widget
                // about the incorrectly formatted message, but it does not look
                // like we have a well-defined error mechanism for such cases.
                // Clarify that later.
                warn!("Failed to parse message from widget: {e}");
            }
        }
    }

    Ok(())
}

/// Map that stores pending responses for the outgoing requests (requests that
/// **we** send to the widget).
type PendingResponses = Arc<Mutex<HashMap<String, oneshot::Sender<ToWidgetAction>>>>;

/// Handles interaction with a widget. Essentially, it's a proxy to the widget
/// that adds some additional type-safety and a convenient "higher level" access
/// to the widget (convenient `async` function that receives a request and
/// returns once the response is received, validated and matched, etc).
pub(crate) struct WidgetProxy {
    /// Widget settings.
    info: WidgetInfo,
    /// Raw communication channel to send `String`s (JSON) to the widget.
    sink: Sender<String>,
    /// Map that stores pending responses for the **outgoing requests**
    /// (requests that **we** send to the widget).
    pending: PendingResponses,
}

impl WidgetProxy {
    fn new(info: WidgetInfo, sink: Sender<String>, pending: PendingResponses) -> Self {
        Self { info, sink, pending }
    }

    /// Sends a request to the widget, returns the response from the widget once
    /// received.
    async fn send<T: OutgoingRequest>(&self, msg: T) -> Result<T::Response> {
        let id = Uuid::new_v4().to_string();
        let message = {
            let header = Header::new(&id, &self.info.id);
            let action = Action::ToWidget(msg.into_action());
            to_json(&Message::new(header, action)).expect("Bug: can't serialise a message")
        };
        self.sink.send(message).await.map_err(|_| Error::WidgetDisconnected)?;

        let (tx, rx) = oneshot::channel();
        self.pending.lock().expect("Pending mutex poisoned").insert(id, tx);
        let reply = rx.await.map_err(|_| Error::WidgetDisconnected)?;
        T::extract_response(reply)
            .ok_or(Error::custom("Widget sent invalid response"))?
            .map_err(Error::WidgetErrorReply)
    }

    /// Sends a reply to one of the incoming requests from the widget. The reply
    /// itself can only be constructed from a valid incoming request, so we
    /// ensure that only valid replies could be constructed. Error is returned
    /// if the reply cannot be sent due to the widget being disconnected.
    async fn reply(&self, response: IncomingResponse) -> StdResult<(), ()> {
        let message: Message = response.into();
        let json = to_json(&message).expect("Bug: can't serialise a message");
        self.sink.send(json).await.map_err(|_| ())
    }

    /// Tells whether or not we should negotiate supported capabilities on
    /// `ContentLoad` or not (if `false`, they are negotiated right away).
    fn init_on_load(&self) -> bool {
        self.info.init_on_load
    }
}
