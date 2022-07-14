use anyhow::{anyhow, Error};
use anyhow::{bail, Context};
use futures::prelude::*;
use futures::ready;
use pin_project_lite::pin_project;
use std::collections::{HashMap, VecDeque};
use std::pin::Pin;
use std::task::{Context as TaskContext, Poll};
use tracing::log::error;
use tracing::{info, instrument, warn};
use webrtcsink_protocol as p;

type PeerId = String;

#[derive(Clone)]
struct Session {
    id: String,
    producer: PeerId,
    consumer: PeerId,
}

impl Session {
    fn other_peer_id(&self, id: &str) -> Result<&str, Error> {
        if self.producer == id {
            Ok(&self.consumer)
        } else if self.consumer == id {
            Ok(&self.producer)
        } else {
            bail!("Peer {id} is not part of {}", self.id)
        }
    }
}

pin_project! {
    #[must_use = "streams do nothing unless polled"]
    pub struct Handler {
        #[pin]
        stream: Pin<Box<dyn Stream<Item=(String, Option<p::IncomingMessage>)> + Send>>,
        items: VecDeque<(String, p::OutgoingMessage)>,
        producers: HashMap<PeerId, Option<serde_json::Value>>,
        consumers: HashMap<PeerId, Option<serde_json::Value>>,
        listeners: HashMap<PeerId, Option<serde_json::Value>>,
        sessions: HashMap<String, Session>,
    }
}

impl Handler {
    #[instrument(level = "debug", skip(stream))]
    /// Create a handler
    pub fn new(
        stream: Pin<Box<dyn Stream<Item = (String, Option<p::IncomingMessage>)> + Send>>,
    ) -> Self {
        Self {
            stream,
            items: VecDeque::new(),
            producers: Default::default(),
            consumers: Default::default(),
            listeners: Default::default(),
            sessions: Default::default(),
        }
    }

    #[instrument(level = "trace", skip(self))]
    fn handle(
        mut self: Pin<&mut Self>,
        peer_id: &str,
        msg: p::IncomingMessage,
    ) -> Result<(), Error> {
        match msg {
            p::IncomingMessage::Register(message) => match message {
                p::RegisterMessage::Producer { meta } => self.register_producer(peer_id, meta),
                p::RegisterMessage::Consumer { meta } => self.register_consumer(peer_id, meta),
                p::RegisterMessage::Listener { meta } => self.register_listener(peer_id, meta),
            },
            p::IncomingMessage::Unregister(message) => {
                let answer = match message {
                    p::UnregisterMessage::Producer => p::UnregisteredMessage::Producer {
                        peer_id: peer_id.into(),
                        meta: self.remove_producer_peer(peer_id),
                    },
                    p::UnregisterMessage::Consumer => p::UnregisteredMessage::Consumer {
                        peer_id: peer_id.into(),
                        meta: self.remove_consumer_peer(peer_id),
                    },
                    p::UnregisterMessage::Listener => p::UnregisteredMessage::Listener {
                        peer_id: peer_id.into(),
                        meta: self.remove_listener_peer(peer_id),
                    },
                };

                self.items.push_back((
                    peer_id.into(),
                    p::OutgoingMessage::Unregistered(answer.clone()),
                ));

                // We don't notify listeners about listeners activity
                match message {
                    p::UnregisterMessage::Producer | p::UnregisterMessage::Consumer => {
                        let mut messages = self
                            .listeners
                            .keys()
                            .map(|listener_id| {
                                (
                                    listener_id.to_string(),
                                    p::OutgoingMessage::Unregistered(answer.clone()),
                                )
                            })
                            .collect::<VecDeque<(String, p::OutgoingMessage)>>();

                        self.items.append(&mut messages);
                    }
                    _ => (),
                }

                Ok(())
            }
            p::IncomingMessage::StartSession(message) => {
                self.start_session(&message.peer_id, peer_id)
            }
            p::IncomingMessage::Peer(peermsg) => self.handle_peer_message(peer_id, peermsg),
            p::IncomingMessage::List => self.list_producers(peer_id),
            p::IncomingMessage::EndSession(msg) => self.end_session(peer_id, &msg.session_id),
        }
    }

    fn handle_peer_message(&mut self, peer_id: &str, peermsg: p::PeerMessage) -> Result<(), Error> {
        let session_id = &peermsg.session_id;
        let session = self
            .sessions
            .get(session_id)
            .context(format!("Session {} doesn't exist", session_id))?
            .clone();

        if matches!(
            peermsg.peer_message,
            p::PeerMessageInner::Sdp(p::SdpMessage::Offer { .. })
        ) {
            if peer_id == session.consumer {
                bail!(
                    r#"cannot forward offer from "{peer_id}" to "{}" as "{peer_id}" is not the producer"#,
                    session.producer,
                );
            }
        }

        self.items.push_back((
            session.other_peer_id(peer_id)?.to_owned(),
            p::OutgoingMessage::Peer(p::PeerMessage {
                session_id: session_id.to_string(),
                peer_message: peermsg.peer_message.clone(),
            }),
        ));

        Ok(())
    }

    #[instrument(level = "debug", skip(self))]
    fn remove_listener_peer(&mut self, peer_id: &str) -> Option<serde_json::Value> {
        self.listeners.remove(peer_id).unwrap_or(None)
    }

    #[instrument(level = "debug", skip(self))]
    /// Remove a peer, this can cause sessions to be ended
    fn remove_peer(&mut self, peer_id: &str) {
        info!(peer_id = %peer_id, "removing peer");

        self.remove_listener_peer(peer_id);
        self.remove_producer_peer(peer_id);
        self.remove_consumer_peer(peer_id);
    }

    #[instrument(level = "debug", skip(self))]
    fn remove_producer_peer(&mut self, peer_id: &str) -> Option<serde_json::Value> {
        let sessions_to_end = self
            .sessions
            .iter()
            .filter_map(|(session_id, session)| {
                if session.producer == peer_id {
                    Some(session_id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<String>>();

        sessions_to_end.iter().for_each(|session_id| {
            if let Err(e) = self.end_session(peer_id, session_id) {
                error!("Could not end session {session_id}: {e:?}");
            }
        });

        let meta = self.producers.remove(peer_id);

        for listener in self.listeners.keys() {
            self.items.push_back((
                listener.to_string(),
                p::OutgoingMessage::ProducerRemoved {
                    peer_id: peer_id.to_string(),
                    meta: meta
                        .as_ref()
                        .map_or_else(Default::default, |meta| meta.clone()),
                },
            ));
        }

        meta.unwrap_or(None)
    }

    #[instrument(level = "debug", skip(self))]
    fn remove_consumer_peer(&mut self, peer_id: &str) -> Option<serde_json::Value> {
        let sessions_to_end = self
            .sessions
            .iter()
            .filter_map(|(session_id, session)| {
                if session.consumer == peer_id {
                    Some(session_id.clone())
                } else {
                    None
                }
            })
            .collect::<Vec<String>>();

        sessions_to_end.iter().for_each(|session_id| {
            if let Err(e) = self.end_session(peer_id, session_id) {
                error!("Could not end session {session_id}: {e:?}");
            }
        });

        self.consumers.remove(peer_id).unwrap_or(None)
    }

    #[instrument(level = "debug", skip(self))]
    /// End a session between two peers
    fn end_session(&mut self, peer_id: &str, session_id: &str) -> Result<(), Error> {
        let session = self
            .sessions
            .remove(session_id)
            .with_context(|| format!("Session {session_id} doesn't exist"))?;

        self.items.push_back((
            session.other_peer_id(peer_id)?.to_string(),
            p::OutgoingMessage::EndSession(p::EndSessionMessage {
                session_id: session_id.to_string(),
            }),
        ));

        Ok(())
    }

    /// List producer peers
    #[instrument(level = "debug", skip(self))]
    fn list_producers(&mut self, peer_id: &str) -> Result<(), Error> {
        self.items.push_back((
            peer_id.to_string(),
            p::OutgoingMessage::List {
                producers: self
                    .producers
                    .iter()
                    .map(|(peer_id, meta)| p::Peer {
                        id: peer_id.clone(),
                        meta: meta.clone(),
                    })
                    .collect(),
            },
        ));

        Ok(())
    }

    /// Register peer as a producer
    #[instrument(level = "debug", skip(self))]
    fn register_producer(
        &mut self,
        peer_id: &str,
        meta: Option<serde_json::Value>,
    ) -> Result<(), Error> {
        if self.producers.contains_key(peer_id) {
            Err(anyhow!("{} is already registered as a producer", peer_id))
        } else {
            self.producers.insert(peer_id.to_string(), meta.clone());

            for listener in self.listeners.keys() {
                self.items.push_back((
                    listener.to_string(),
                    p::OutgoingMessage::ProducerAdded {
                        peer_id: peer_id.to_string(),
                        meta: meta.clone(),
                    },
                ));
            }

            self.items.push_back((
                peer_id.to_string(),
                p::OutgoingMessage::Registered(p::RegisteredMessage::Producer {
                    peer_id: peer_id.to_string(),
                    meta: meta,
                }),
            ));

            info!(peer_id = %peer_id, "registered as a producer");

            Ok(())
        }
    }

    /// Register peer as a consumer
    #[instrument(level = "debug", skip(self))]
    fn register_consumer(
        &mut self,
        peer_id: &str,
        meta: Option<serde_json::Value>,
    ) -> Result<(), Error> {
        if self.consumers.contains_key(peer_id) {
            Err(anyhow!("{} is already registered as a consumer", peer_id))
        } else {
            self.consumers.insert(peer_id.to_string(), meta.clone());

            self.items.push_back((
                peer_id.to_string(),
                p::OutgoingMessage::Registered(p::RegisteredMessage::Consumer {
                    peer_id: peer_id.to_string(),
                    meta: meta.clone(),
                }),
            ));

            info!(peer_id = %peer_id, "registered as a consumer");

            Ok(())
        }
    }

    /// Register peer as a listener
    #[instrument(level = "debug", skip(self))]
    fn register_listener(
        &mut self,
        peer_id: &str,
        meta: Option<serde_json::Value>,
    ) -> Result<(), Error> {
        if self.listeners.contains_key(peer_id) {
            bail!("{} is already registered as a listener", peer_id);
        }

        self.listeners.insert(peer_id.to_string(), meta.clone());
        self.items.push_back((
            peer_id.to_string(),
            p::OutgoingMessage::Registered(p::RegisteredMessage::Listener {
                peer_id: peer_id.to_string(),
                meta: meta.clone(),
            }),
        ));

        info!(peer_id = %peer_id, "registered as a listener");

        Ok(())
    }

    /// Start a session between two peers
    #[instrument(level = "debug", skip(self))]
    fn start_session(&mut self, producer_id: &str, consumer_id: &str) -> Result<(), Error> {
        if !self.consumers.contains_key(consumer_id) {
            return Err(anyhow!(
                "Peer with id {} is not registered as a consumer",
                consumer_id
            ));
        }

        if !self.producers.contains_key(producer_id) {
            return Err(anyhow!(
                "Peer with id {} is not registered as a producer",
                producer_id
            ));
        }

        let session_id = uuid::Uuid::new_v4().to_string();
        self.sessions.insert(
            session_id.clone(),
            Session {
                id: session_id.clone(),
                consumer: consumer_id.to_string(),
                producer: producer_id.to_string(),
            },
        );
        self.items.push_back((
            consumer_id.to_string(),
            p::OutgoingMessage::SessionStarted {
                peer_id: producer_id.to_string(),
                session_id: session_id.clone(),
            },
        ));
        self.items.push_back((
            producer_id.to_string(),
            p::OutgoingMessage::StartSession {
                peer_id: consumer_id.to_string(),
                session_id: session_id.clone(),
            },
        ));

        info!(id = %session_id, producer_id = %producer_id, consumer_id = %consumer_id, "started a session");

        Ok(())
    }
}

impl Stream for Handler {
    type Item = (String, p::OutgoingMessage);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        loop {
            let this = self.as_mut().project();

            if let Some(item) = this.items.pop_front() {
                break Poll::Ready(Some(item));
            }

            match ready!(this.stream.poll_next(cx)) {
                Some((peer_id, msg)) => {
                    if let Some(msg) = msg {
                        if let Err(err) = self.as_mut().handle(&peer_id, msg) {
                            self.items.push_back((
                                peer_id.to_string(),
                                p::OutgoingMessage::Error {
                                    details: err.to_string(),
                                },
                            ));
                        }
                    } else {
                        self.remove_peer(&peer_id);
                    }
                }
                None => {
                    break Poll::Ready(None);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::channel::mpsc;
    use serde_json::json;

    #[async_std::test]
    async fn test_register_producer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });

        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Registered(p::RegisteredMessage::Producer {
                peer_id: "producer".to_string(),
                meta: Default::default(),
            })
        );
    }

    #[async_std::test]
    async fn test_list_producers() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Some(json!( {"display-name": "foobar".to_string() })),
        });

        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();

        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::List;

        tx.send(("listener".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::List {
                producers: vec![p::Peer::Producer {
                    id: "producer".to_string(),
                    meta: Some(json!(
                        {"display-name": "foobar".to_string()
                    })),
                }]
            }
        );
    }

    #[async_std::test]
    async fn test_register_consumer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });

        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Registered(p::RegisteredMessage::Consumer {
                peer_id: "consumer".to_string(),
                meta: Default::default()
            })
        );
    }

    #[async_std::test]
    async fn test_register_producer_twice() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Error {
                details: "producer is already registered as a producer".into()
            }
        );
    }

    #[async_std::test]
    async fn test_listener() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Listener {
            meta: Default::default(),
        });
        tx.send(("listener".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Some(json!({
                "display-name": "foobar".to_string(),
            })),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::ProducerAdded {
                peer_id: "producer".to_string(),
                meta: Some(json!({
                    "display-name": Some("foobar".to_string()),
                }))
            }
        );
    }

    #[async_std::test]
    async fn test_start_session() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");
        let session_id = match sent_message {
            p::OutgoingMessage::SessionStarted {
                ref peer_id,
                ref session_id,
            } => {
                assert_eq!(peer_id, "producer");
                session_id.to_string()
            }
            _ => panic!("SessionStarted message missing"),
        };

        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::StartSession {
                peer_id: "consumer".to_string(),
                session_id: session_id.to_string(),
            }
        );
    }

    #[async_std::test]
    async fn test_remove_peer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");
        let session_id = match sent_message {
            p::OutgoingMessage::SessionStarted {
                ref peer_id,
                ref session_id,
            } => {
                assert_eq!(peer_id, "producer");
                session_id.to_string()
            }
            _ => panic!("SessionStarted message missing"),
        };

        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Listener {
            meta: Default::default(),
        });
        tx.send(("listener".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        handler.remove_peer("producer");
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession(p::EndSessionMessage { session_id })
        );

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::ProducerRemoved {
                peer_id: "producer".to_string(),
                meta: Default::default()
            }
        );
    }

    #[async_std::test]
    async fn test_end_session_consumer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");
        let session_id = match sent_message {
            p::OutgoingMessage::SessionStarted {
                ref peer_id,
                ref session_id,
            } => {
                assert_eq!(peer_id, "producer");
                session_id.to_string()
            }
            _ => panic!("SessionStarted message missing"),
        };

        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage {
            session_id: session_id.clone(),
        });

        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession(p::EndSessionMessage {
                session_id: session_id
            })
        );
    }

    #[async_std::test]
    async fn test_end_session_producer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");
        let session_id = match sent_message {
            p::OutgoingMessage::SessionStarted {
                ref peer_id,
                ref session_id,
            } => {
                assert_eq!(peer_id, "producer");
                session_id.to_string()
            }
            _ => panic!("SessionStarted message missing"),
        };

        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage {
            session_id: session_id.clone(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession(p::EndSessionMessage { session_id })
        );
    }

    #[async_std::test]
    async fn test_end_session_twice() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");
        let session_id = match sent_message {
            p::OutgoingMessage::SessionStarted {
                ref peer_id,
                ref session_id,
            } => {
                assert_eq!(peer_id, "producer");
                session_id.to_string()
            }
            _ => panic!("SessionStarted message missing"),
        };

        let _ = handler.next().await.unwrap();

        // The consumer ends the session
        let message = p::IncomingMessage::EndSession(p::EndSessionMessage {
            session_id: session_id.clone(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession(p::EndSessionMessage {
                session_id: session_id.clone()
            })
        );

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage {
            session_id: session_id.clone(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Error {
                details: format!("Session {session_id} doesn't exist")
            }
        );
    }

    #[async_std::test]
    async fn test_sdp_exchange() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");
        let session_id = match sent_message {
            p::OutgoingMessage::SessionStarted {
                ref peer_id,
                ref session_id,
            } => {
                assert_eq!(peer_id, "producer");
                session_id.to_string()
            }
            _ => panic!("SessionStarted message missing"),
        };

        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Peer(p::PeerMessage {
            session_id: session_id.clone(),
            peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                sdp: "offer".to_string(),
            }),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Peer(p::PeerMessage {
                session_id: session_id.clone(),
                peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                    sdp: "offer".to_string()
                })
            })
        );
    }

    #[async_std::test]
    async fn test_ice_exchange() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");
        let session_id = match sent_message {
            p::OutgoingMessage::SessionStarted {
                ref peer_id,
                ref session_id,
            } => {
                assert_eq!(peer_id, "producer");
                session_id.to_string()
            }
            _ => panic!("SessionStarted message missing"),
        };

        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Peer(p::PeerMessage {
            session_id: session_id.clone(),
            peer_message: p::PeerMessageInner::Ice {
                candidate: "candidate".to_string(),
                sdp_m_line_index: 42,
            },
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Peer(p::PeerMessage {
                session_id: session_id.clone(),
                peer_message: p::PeerMessageInner::Ice {
                    candidate: "candidate".to_string(),
                    sdp_m_line_index: 42
                }
            })
        );

        let message = p::IncomingMessage::Peer(p::PeerMessage {
            session_id: session_id.clone(),
            peer_message: p::PeerMessageInner::Ice {
                candidate: "candidate".to_string(),
                sdp_m_line_index: 42,
            },
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Peer(p::PeerMessage {
                session_id: session_id.clone(),
                peer_message: p::PeerMessageInner::Ice {
                    candidate: "candidate".to_string(),
                    sdp_m_line_index: 42
                }
            })
        );
    }

    #[async_std::test]
    async fn test_sdp_exchange_wrong_direction_offer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");
        let session_id = match sent_message {
            p::OutgoingMessage::SessionStarted {
                ref peer_id,
                ref session_id,
            } => {
                assert_eq!(peer_id, "producer");
                session_id.to_string()
            }
            _ => panic!("SessionStarted message missing"),
        };

        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Peer(p::PeerMessage {
            session_id,
            peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                sdp: "offer".to_string(),
            }),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        // assert_eq!(peer_id, "consumer");
        assert_eq!(sent_message,
            p::OutgoingMessage::Error {
                details: r#"cannot forward offer from "consumer" to "producer" as "consumer" is not the producer"#.into()
            }
        );
    }

    #[async_std::test]
    async fn test_start_session_no_producer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Error {
                details: "Peer with id producer is not registered as a producer".into()
            }
        );
    }

    #[async_std::test]
    async fn test_unregistering() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");
        let session_id = match sent_message {
            p::OutgoingMessage::SessionStarted {
                ref peer_id,
                ref session_id,
            } => {
                assert_eq!(peer_id, "producer");
                session_id.to_string()
            }
            _ => panic!("SessionStarted message missing"),
        };

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::StartSession {
                peer_id: "consumer".to_string(),
                session_id: session_id.clone(),
            }
        );

        let message = p::IncomingMessage::Unregister(p::UnregisterMessage::Producer);
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession(p::EndSessionMessage {
                session_id: session_id.clone(),
            })
        );

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Unregistered(p::UnregisteredMessage::Producer {
                peer_id: "producer".into(),
                meta: Default::default()
            })
        );
    }

    #[async_std::test]
    async fn test_unregistering_with_listenners() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Listener {
            meta: Default::default(),
        });
        tx.send(("listener".to_string(), Some(message)))
            .await
            .unwrap();
        let (l, _) = handler.next().await.unwrap();
        assert_eq!(l, "listener");

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Some(json!({"some": "meta"})),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::ProducerAdded {
                peer_id: "producer".to_string(),
                meta: Some(json!({"some": "meta"})),
            }
        );

        let (peer_id, _msg) = handler.next().await.unwrap();
        assert_eq!(peer_id, "producer");

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, _msg) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");
        let session_id = match sent_message {
            p::OutgoingMessage::SessionStarted {
                ref peer_id,
                ref session_id,
            } => {
                assert_eq!(peer_id, "producer");
                session_id.to_string()
            }
            _ => panic!("SessionStarted message missing"),
        };

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::StartSession {
                peer_id: "consumer".to_string(),
                session_id: session_id.clone(),
            }
        );

        let message = p::IncomingMessage::Unregister(p::UnregisterMessage::Producer);
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession(p::EndSessionMessage {
                session_id: session_id.clone(),
            })
        );

        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::ProducerRemoved {
                peer_id: "producer".into(),
                meta: Some(json!({"some": "meta"}))
            }
        );
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Unregistered(p::UnregisteredMessage::Producer {
                peer_id: "producer".into(),
                meta: Some(json!({"some": "meta"}))
            })
        );
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Unregistered(p::UnregisteredMessage::Producer {
                peer_id: "producer".into(),
                meta: Some(json!({"some": "meta"})),
            })
        );
    }

    #[async_std::test]
    async fn test_start_session_no_consumer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Error {
                details: "Peer with id consumer is not registered as a consumer".into()
            }
        );
    }

    #[async_std::test]
    async fn test_start_session_twice() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            meta: Default::default(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Consumer {
            meta: Default::default(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");
        let session0_id = match sent_message {
            p::OutgoingMessage::SessionStarted {
                ref peer_id,
                ref session_id,
            } => {
                assert_eq!(peer_id, "producer");
                session_id.to_string()
            }
            _ => panic!("SessionStarted message missing"),
        };

        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::StartSession(p::StartSessionMessage {
            peer_id: "producer".to_string(),
        });

        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();
        assert_eq!(peer_id, "consumer");
        let session1_id = match sent_message {
            p::OutgoingMessage::SessionStarted {
                ref peer_id,
                ref session_id,
            } => {
                assert_eq!(peer_id, "producer");
                session_id.to_string()
            }
            _ => panic!("SessionStarted message missing"),
        };

        assert_ne!(session0_id, session1_id);
    }
}
