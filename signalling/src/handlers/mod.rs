use anyhow::{anyhow, Error};
use futures::prelude::*;
use futures::ready;
use pin_project_lite::pin_project;
use std::collections::{HashMap, HashSet, VecDeque};
use std::pin::Pin;
use std::task::{Context, Poll};
use tracing::{info, instrument, warn};
use webrtcsink_protocol as p;

type PeerId = String;

pin_project! {
    #[must_use = "streams do nothing unless polled"]
    pub struct Handler {
        #[pin]
        stream: Pin<Box<dyn Stream<Item=(String, Option<p::IncomingMessage>)> + Send>>,
        items: VecDeque<(String, p::OutgoingMessage)>,
        producers: HashMap<PeerId, HashSet<PeerId>>,
        consumers: HashMap<PeerId, Option<PeerId>>,
        listeners: HashSet<PeerId>,
        display_names: HashMap<PeerId, Option<String>>,
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
            producers: HashMap::new(),
            consumers: HashMap::new(),
            listeners: HashSet::new(),
            display_names: HashMap::new(),
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
                p::RegisterMessage::Producer { display_name } => {
                    self.register_producer(peer_id, display_name)
                }
                p::RegisterMessage::Consumer { display_name } => {
                    self.register_consumer(peer_id, display_name)
                }
                p::RegisterMessage::Listener { display_name } => {
                    self.register_listener(peer_id, display_name)
                }
            },
            p::IncomingMessage::StartSession(message) => {
                self.start_session(&message.peer_id, peer_id)
            }
            p::IncomingMessage::Peer(p::PeerMessage {
                peer_id: other_peer_id,
                peer_message,
            }) => match peer_message {
                p::PeerMessageInner::Ice {
                    candidate,
                    sdp_m_line_index,
                } => self.handle_ice(candidate, sdp_m_line_index, peer_id, &other_peer_id),
                p::PeerMessageInner::Sdp(sdp_message) => match sdp_message {
                    p::SdpMessage::Offer { sdp } => {
                        self.handle_sdp_offer(sdp, peer_id, &other_peer_id)
                    }
                    p::SdpMessage::Answer { sdp } => {
                        self.handle_sdp_answer(sdp, peer_id, &other_peer_id)
                    }
                },
            },
            p::IncomingMessage::List => self.list_producers(peer_id),
            p::IncomingMessage::EndSession(p::EndSessionMessage {
                peer_id: other_peer_id,
            }) => self.end_session(peer_id, &other_peer_id),
        }
    }

    #[instrument(level = "debug", skip(self))]
    /// Remove a peer, this can cause sessions to be ended
    fn remove_peer(&mut self, peer_id: &str) {
        info!(peer_id = %peer_id, "removing peer");

        self.listeners.remove(peer_id);

        if let Some(consumers) = self.producers.remove(peer_id) {
            for consumer_id in &consumers {
                info!(producer_id=%peer_id, consumer_id=%consumer_id, "ended session");
                self.consumers.insert(consumer_id.clone(), None);
                self.items.push_back((
                    consumer_id.to_string(),
                    p::OutgoingMessage::EndSession {
                        peer_id: peer_id.to_string(),
                    },
                ));
            }

            for listener in &self.listeners {
                self.items.push_back((
                    listener.to_string(),
                    p::OutgoingMessage::ProducerRemoved {
                        peer_id: peer_id.to_string(),
                        display_name: match self.display_names.get(peer_id) {
                            Some(name) => name.clone(),
                            None => None,
                        },
                    },
                ));
            }
        }

        if let Some(Some(producer_id)) = self.consumers.remove(peer_id) {
            info!(producer_id=%producer_id, consumer_id=%peer_id, "ended session");

            self.producers
                .get_mut(&producer_id)
                .unwrap()
                .remove(peer_id);

            self.items.push_back((
                producer_id.to_string(),
                p::OutgoingMessage::EndSession {
                    peer_id: peer_id.to_string(),
                },
            ));
        }

        let _ = self.display_names.remove(peer_id);
    }

    #[instrument(level = "debug", skip(self))]
    /// End a session between two peers
    fn end_session(&mut self, peer_id: &str, other_peer_id: &str) -> Result<(), Error> {
        info!(peer_id=%peer_id, other_peer_id=%other_peer_id, "endsession request");
        if let Some(ref mut consumers) = self.producers.get_mut(peer_id) {
            if consumers.remove(other_peer_id) {
                info!(producer_id=%peer_id, consumer_id=%other_peer_id, "ended session");

                self.items.push_back((
                    other_peer_id.to_string(),
                    p::OutgoingMessage::EndSession {
                        peer_id: peer_id.to_string(),
                    },
                ));

                self.consumers.insert(other_peer_id.to_string(), None);
                Ok(())
            } else {
                Err(anyhow!(
                    "Producer {} has no consumer {}",
                    peer_id,
                    other_peer_id
                ))
            }
        } else if let Some(Some(producer_id)) = self.consumers.get(peer_id) {
            if producer_id == other_peer_id {
                info!(producer_id=%other_peer_id, consumer_id=%peer_id, "ended session");

                self.consumers.insert(peer_id.to_string(), None);
                self.producers
                    .get_mut(other_peer_id)
                    .unwrap()
                    .remove(peer_id);

                self.items.push_back((
                    other_peer_id.to_string(),
                    p::OutgoingMessage::EndSession {
                        peer_id: peer_id.to_string(),
                    },
                ));

                Ok(())
            } else {
                Err(anyhow!(
                    "Consumer {} is not in a session with {}",
                    peer_id,
                    other_peer_id
                ))
            }
        } else {
            Err(anyhow!(
                "No session between {} and {}",
                peer_id,
                other_peer_id
            ))
        }
    }

    /// List producer peers
    #[instrument(level = "debug", skip(self))]
    fn list_producers(&mut self, peer_id: &str) -> Result<(), Error> {
        self.items.push_back((
            peer_id.to_string(),
            p::OutgoingMessage::List {
                producers: self
                    .producers
                    .keys()
                    .cloned()
                    .map(|peer_id| p::Peer {
                        id: peer_id.clone(),
                        display_name: match self.display_names.get(&peer_id) {
                            Some(name) => name.clone(),
                            None => None,
                        },
                    })
                    .collect(),
            },
        ));

        Ok(())
    }

    /// Handle ICE candidate sent by one peer to another peer
    #[instrument(level = "debug", skip(self))]
    fn handle_ice(
        &mut self,
        candidate: String,
        sdp_m_line_index: u32,
        peer_id: &str,
        other_peer_id: &str,
    ) -> Result<(), Error> {
        if let Some(consumers) = self.producers.get(peer_id) {
            if consumers.contains(other_peer_id) {
                self.items.push_back((
                    other_peer_id.to_string(),
                    p::OutgoingMessage::Peer(p::PeerMessage {
                        peer_id: peer_id.to_string(),
                        peer_message: p::PeerMessageInner::Ice {
                            candidate,
                            sdp_m_line_index,
                        },
                    }),
                ));
                Ok(())
            } else {
                Err(anyhow!(
                    "cannot forward ICE from {} to {} as they are not in a session",
                    peer_id,
                    other_peer_id
                ))
            }
        } else if let Some(producer) = self.consumers.get(peer_id) {
            if &Some(other_peer_id.to_string()) == producer {
                self.items.push_back((
                    other_peer_id.to_string(),
                    p::OutgoingMessage::Peer(p::PeerMessage {
                        peer_id: peer_id.to_string(),
                        peer_message: p::PeerMessageInner::Ice {
                            candidate,
                            sdp_m_line_index,
                        },
                    }),
                ));

                Ok(())
            } else {
                Err(anyhow!(
                    "cannot forward ICE from {} to {} as they are not in a session",
                    peer_id,
                    other_peer_id
                ))
            }
        } else {
            Err(anyhow!(
                "cannot forward ICE from {} to {} as they are not in a session",
                peer_id,
                other_peer_id,
            ))
        }
    }

    /// Handle SDP offered by one peer to another peer
    #[instrument(level = "debug", skip(self))]
    fn handle_sdp_offer(
        &mut self,
        sdp: String,
        producer_id: &str,
        consumer_id: &str,
    ) -> Result<(), Error> {
        if let Some(consumers) = self.producers.get(producer_id) {
            if consumers.contains(consumer_id) {
                self.items.push_back((
                    consumer_id.to_string(),
                    p::OutgoingMessage::Peer(p::PeerMessage {
                        peer_id: producer_id.to_string(),
                        peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer { sdp }),
                    }),
                ));
                Ok(())
            } else {
                Err(anyhow!(
                    "cannot forward offer from {} to {} as they are not in a session",
                    producer_id,
                    consumer_id
                ))
            }
        } else {
            Err(anyhow!(
                "cannot forward offer from {} to {} as they are not in a session or {} is not the producer",
                producer_id,
                consumer_id,
                producer_id,
            ))
        }
    }

    /// Handle the SDP answer from one peer to another peer
    #[instrument(level = "debug", skip(self))]
    fn handle_sdp_answer(
        &mut self,
        sdp: String,
        consumer_id: &str,
        producer_id: &str,
    ) -> Result<(), Error> {
        if let Some(producer) = self.consumers.get(consumer_id) {
            if &Some(producer_id.to_string()) == producer {
                self.items.push_back((
                    producer_id.to_string(),
                    p::OutgoingMessage::Peer(p::PeerMessage {
                        peer_id: consumer_id.to_string(),
                        peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Answer { sdp }),
                    }),
                ));
                Ok(())
            } else {
                Err(anyhow!(
                    "cannot forward answer from {} to {} as they are not in a session",
                    consumer_id,
                    producer_id
                ))
            }
        } else {
            Err(anyhow!(
                "cannot forward answer from {} to {} as they are not in a session",
                consumer_id,
                producer_id
            ))
        }
    }

    /// Register peer as a producer
    #[instrument(level = "debug", skip(self))]
    fn register_producer(
        &mut self,
        peer_id: &str,
        display_name: Option<String>,
    ) -> Result<(), Error> {
        if self.producers.contains_key(peer_id) {
            Err(anyhow!("{} is already registered as a producer", peer_id))
        } else {
            self.producers.insert(peer_id.to_string(), HashSet::new());

            for listener in &self.listeners {
                self.items.push_back((
                    listener.to_string(),
                    p::OutgoingMessage::ProducerAdded {
                        peer_id: peer_id.to_string(),
                        display_name: display_name.clone(),
                    },
                ));
            }

            self.items.push_back((
                peer_id.to_string(),
                p::OutgoingMessage::Registered(p::RegisteredMessage::Producer {
                    peer_id: peer_id.to_string(),
                    display_name: display_name.clone(),
                }),
            ));

            self.display_names.insert(peer_id.to_string(), display_name);

            info!(peer_id = %peer_id, "registered as a producer");

            Ok(())
        }
    }

    /// Register peer as a consumer
    #[instrument(level = "debug", skip(self))]
    fn register_consumer(
        &mut self,
        peer_id: &str,
        display_name: Option<String>,
    ) -> Result<(), Error> {
        if self.consumers.contains_key(peer_id) {
            Err(anyhow!("{} is already registered as a consumer", peer_id))
        } else {
            self.consumers.insert(peer_id.to_string(), None);

            self.items.push_back((
                peer_id.to_string(),
                p::OutgoingMessage::Registered(p::RegisteredMessage::Consumer {
                    peer_id: peer_id.to_string(),
                    display_name: display_name.clone(),
                }),
            ));

            self.display_names.insert(peer_id.to_string(), display_name);

            info!(peer_id = %peer_id, "registered as a consumer");

            Ok(())
        }
    }

    /// Register peer as a listener
    #[instrument(level = "debug", skip(self))]
    fn register_listener(
        &mut self,
        peer_id: &str,
        display_name: Option<String>,
    ) -> Result<(), Error> {
        if !self.listeners.insert(peer_id.to_string()) {
            Err(anyhow!("{} is already registered as a listener", peer_id))
        } else {
            self.items.push_back((
                peer_id.to_string(),
                p::OutgoingMessage::Registered(p::RegisteredMessage::Listener {
                    peer_id: peer_id.to_string(),
                    display_name: display_name.clone(),
                }),
            ));

            self.display_names.insert(peer_id.to_string(), display_name);

            info!(peer_id = %peer_id, "registered as a listener");

            Ok(())
        }
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

        if let Some(producer_id) = self.consumers.get(consumer_id).unwrap() {
            return Err(anyhow!(
                "Consumer with id {} is already in a session with producer {}",
                consumer_id,
                producer_id,
            ));
        }

        if !self.producers.contains_key(producer_id) {
            return Err(anyhow!(
                "Peer with id {} is not registered as a producer",
                producer_id
            ));
        }

        self.consumers
            .insert(consumer_id.to_string(), Some(producer_id.to_string()));
        self.producers
            .get_mut(producer_id)
            .unwrap()
            .insert(consumer_id.to_string());

        self.items.push_back((
            producer_id.to_string(),
            p::OutgoingMessage::StartSession {
                peer_id: consumer_id.to_string(),
            },
        ));

        info!(producer_id = %producer_id, consumer_id = %consumer_id, "started a session");

        Ok(())
    }
}

impl Stream for Handler {
    type Item = (String, p::OutgoingMessage);

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
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

    #[async_std::test]
    async fn test_register_producer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });

        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Registered(p::RegisteredMessage::Producer {
                peer_id: "producer".to_string(),
                display_name: None,
            })
        );
    }

    #[async_std::test]
    async fn test_list_producers() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            display_name: Some("foobar".to_string()),
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
                producers: vec![p::Peer {
                    id: "producer".to_string(),
                    display_name: Some("foobar".to_string())
                }]
            }
        );
    }

    #[async_std::test]
    async fn test_register_consumer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Consumer { display_name: None });

        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Registered(p::RegisteredMessage::Consumer {
                peer_id: "consumer".to_string(),
                display_name: None,
            })
        );
    }

    #[async_std::test]
    async fn test_register_producer_twice() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });
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

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Listener { display_name: None });
        tx.send(("listener".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Register(p::RegisterMessage::Producer {
            display_name: Some("foobar".to_string()),
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
                display_name: Some("foobar".to_string()),
            }
        );
    }

    #[async_std::test]
    async fn test_start_session() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Consumer { display_name: None });
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

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::StartSession {
                peer_id: "consumer".to_string()
            }
        );
    }

    #[async_std::test]
    async fn test_remove_peer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Consumer { display_name: None });
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
        let _ = handler.next().await.unwrap();

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Listener { display_name: None });
        tx.send(("listener".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        handler.remove_peer("producer");
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession {
                peer_id: "producer".to_string()
            }
        );

        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "listener");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::ProducerRemoved {
                peer_id: "producer".to_string(),
                display_name: None
            }
        );
    }

    #[async_std::test]
    async fn test_end_session_consumer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Consumer { display_name: None });
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
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage {
            peer_id: "producer".to_string(),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession {
                peer_id: "consumer".to_string()
            }
        );
    }

    #[async_std::test]
    async fn test_end_session_producer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Consumer { display_name: None });
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
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage {
            peer_id: "consumer".to_string(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::EndSession {
                peer_id: "producer".to_string()
            }
        );
    }

    #[async_std::test]
    async fn test_end_session_twice() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Consumer { display_name: None });
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
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage {
            peer_id: "consumer".to_string(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::EndSession(p::EndSessionMessage {
            peer_id: "consumer".to_string(),
        });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "producer");
        assert_eq!(
            sent_message,
            p::OutgoingMessage::Error {
                details: "Producer producer has no consumer consumer".into()
            }
        );
    }

    #[async_std::test]
    async fn test_sdp_exchange() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Consumer { display_name: None });
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
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Peer(p::PeerMessage {
            peer_id: "consumer".to_string(),
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
                peer_id: "producer".to_string(),
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

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Consumer { display_name: None });
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
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Peer(p::PeerMessage {
            peer_id: "consumer".to_string(),
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
                peer_id: "producer".to_string(),
                peer_message: p::PeerMessageInner::Ice {
                    candidate: "candidate".to_string(),
                    sdp_m_line_index: 42
                }
            })
        );

        let message = p::IncomingMessage::Peer(p::PeerMessage {
            peer_id: "producer".to_string(),
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
                peer_id: "consumer".to_string(),
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

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Consumer { display_name: None });
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
        let _ = handler.next().await.unwrap();

        let message = p::IncomingMessage::Peer(p::PeerMessage {
            peer_id: "producer".to_string(),
            peer_message: p::PeerMessageInner::Sdp(p::SdpMessage::Offer {
                sdp: "offer".to_string(),
            }),
        });
        tx.send(("consumer".to_string(), Some(message)))
            .await
            .unwrap();
        let (peer_id, sent_message) = handler.next().await.unwrap();

        assert_eq!(peer_id, "consumer");
        assert_eq!(sent_message,
            p::OutgoingMessage::Error {
                details: "cannot forward offer from consumer to producer as they are not in a session or consumer is not the producer".into()
            }
        );
    }

    #[async_std::test]
    async fn test_start_session_no_producer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Consumer { display_name: None });
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
    async fn test_start_session_no_consumer() {
        let (mut tx, rx) = mpsc::unbounded();
        let mut handler = Handler::new(Box::pin(rx));

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });
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

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Producer { display_name: None });
        tx.send(("producer".to_string(), Some(message)))
            .await
            .unwrap();
        let _ = handler.next().await.unwrap();

        let message =
            p::IncomingMessage::Register(p::RegisterMessage::Consumer { display_name: None });
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
                details: "Consumer with id consumer is already in a session with producer producer"
                    .into()
            }
        );
    }
}
