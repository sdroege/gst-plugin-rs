use anyhow::Error;
use async_std::task;
use async_tungstenite::tungstenite::Message as WsMessage;
use futures::channel::mpsc;
use futures::prelude::*;
use futures::{AsyncRead, AsyncWrite};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use tracing::{info, instrument, trace, warn};

struct Peer {
    receive_task_handle: task::JoinHandle<()>,
    send_task_handle: task::JoinHandle<Result<(), Error>>,
    sender: mpsc::Sender<String>,
}

struct State {
    tx: Option<mpsc::Sender<(String, Option<String>)>>,
    peers: HashMap<String, Peer>,
}

#[derive(Clone)]
pub struct Server {
    state: Arc<Mutex<State>>,
}

#[derive(thiserror::Error, Debug)]
pub enum ServerError {
    #[error("error during handshake {0}")]
    Handshake(#[from] async_tungstenite::tungstenite::Error),
}

impl Server {
    #[instrument(level = "debug", skip(factory))]
    pub fn spawn<
        I: for<'a> Deserialize<'a>,
        O: Serialize + std::fmt::Debug,
        Factory: FnOnce(Pin<Box<dyn Stream<Item = (String, Option<I>)> + Send>>) -> St,
        St: Stream<Item = (String, O)>,
    >(
        factory: Factory,
    ) -> Self
    where
        O: Serialize + std::fmt::Debug,
        St: Send + Unpin + 'static,
    {
        let (tx, rx) = mpsc::channel::<(String, Option<String>)>(1000);
        let mut handler = factory(Box::pin(rx.filter_map(|(peer_id, msg)| async move {
            if let Some(msg) = msg {
                match serde_json::from_str::<I>(&msg) {
                    Ok(msg) => Some((peer_id, Some(msg))),
                    Err(err) => {
                        warn!("Failed to parse incoming message: {} ({})", err, msg);
                        None
                    }
                }
            } else {
                Some((peer_id, None))
            }
        })));

        let state = Arc::new(Mutex::new(State {
            tx: Some(tx),
            peers: HashMap::new(),
        }));

        let state_clone = state.clone();
        let _ = task::spawn(async move {
            while let Some((peer_id, msg)) = handler.next().await {
                match serde_json::to_string(&msg) {
                    Ok(msg) => {
                        if let Some(peer) = state_clone.lock().unwrap().peers.get_mut(&peer_id) {
                            let mut sender = peer.sender.clone();
                            task::spawn(async move {
                                let _ = sender.send(msg).await;
                            });
                        }
                    }
                    Err(err) => {
                        warn!("Failed to serialize outgoing message: {}", err);
                    }
                }
            }
        });

        Self { state }
    }

    #[instrument(level = "debug", skip(state))]
    fn remove_peer(state: Arc<Mutex<State>>, peer_id: &str) {
        if let Some(mut peer) = state.lock().unwrap().peers.remove(peer_id) {
            let peer_id = peer_id.to_string();
            task::spawn(async move {
                peer.sender.close_channel();
                if let Err(err) = peer.send_task_handle.await {
                    trace!(peer_id = %peer_id, "Error while joining send task: {}", err);
                }
                peer.receive_task_handle.await;
            });
        }
    }

    #[instrument(level = "debug", skip(self, stream))]
    pub async fn accept_async<S: 'static>(&mut self, stream: S) -> Result<String, ServerError>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send,
    {
        let ws = match async_tungstenite::accept_async(stream).await {
            Ok(ws) => ws,
            Err(err) => {
                warn!("Error during the websocket handshake: {}", err);
                return Err(ServerError::Handshake(err));
            }
        };

        let this_id = uuid::Uuid::new_v4().to_string();
        info!(this_id = %this_id, "New WebSocket connection");

        // 1000 is completely arbitrary, we simply don't want infinite piling
        // up of messages as with unbounded
        let (websocket_sender, mut websocket_receiver) = mpsc::channel::<String>(1000);

        let this_id_clone = this_id.clone();
        let (mut ws_sink, mut ws_stream) = ws.split();
        let send_task_handle = task::spawn(async move {
            loop {
                match async_std::future::timeout(
                    std::time::Duration::from_secs(30),
                    websocket_receiver.next(),
                )
                .await
                {
                    Ok(Some(msg)) => {
                        trace!(this_id = %this_id_clone, "sending {}", msg);
                        ws_sink.send(WsMessage::Text(msg)).await?;
                    }
                    Ok(None) => {
                        break;
                    }
                    Err(_) => {
                        trace!(this_id = %this_id_clone, "timeout, sending ping");
                        ws_sink.send(WsMessage::Ping(vec![])).await?;
                    }
                }
            }

            ws_sink.send(WsMessage::Close(None)).await?;
            ws_sink.close().await?;

            Ok::<(), Error>(())
        });

        let mut tx = self.state.lock().unwrap().tx.clone();
        let this_id_clone = this_id.clone();
        let state_clone = self.state.clone();
        let receive_task_handle = task::spawn(async move {
            while let Some(msg) = ws_stream.next().await {
                match msg {
                    Ok(WsMessage::Text(msg)) => {
                        if let Some(tx) = tx.as_mut() {
                            if let Err(err) = tx.send((this_id_clone.clone(), Some(msg))).await {
                                warn!(this = %this_id_clone, "Error handling message: {:?}", err);
                            }
                        }
                    }
                    Ok(WsMessage::Close(reason)) => {
                        info!(this_id = %this_id_clone, "connection closed: {:?}", reason);
                        break;
                    }
                    Ok(WsMessage::Pong(_)) => {
                        continue;
                    }
                    Ok(_) => warn!(this_id = %this_id_clone, "Unsupported message type"),
                    Err(err) => {
                        warn!(this_id = %this_id_clone, "recv error: {}", err);
                        break;
                    }
                }
            }

            if let Some(tx) = tx.as_mut() {
                let _ = tx.send((this_id_clone.clone(), None)).await;
            }

            Self::remove_peer(state_clone, &this_id_clone);
        });

        self.state.lock().unwrap().peers.insert(
            this_id.clone(),
            Peer {
                receive_task_handle,
                send_task_handle,
                sender: websocket_sender,
            },
        );

        Ok(this_id)
    }
}
