/// The default protocol used by the signalling server
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "peerType")]
#[serde(rename_all = "camelCase")]
/// Confirms registration
pub enum RegisteredMessage {
    /// Registered as a producer
    #[serde(rename_all = "camelCase")]
    Producer {
        peer_id: String,
        display_name: Option<String>,
    },
    /// Registered as a consumer
    #[serde(rename_all = "camelCase")]
    Consumer {
        peer_id: String,
        display_name: Option<String>,
    },
    /// Registered as a listener
    #[serde(rename_all = "camelCase")]
    Listener {
        peer_id: String,
        display_name: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Peer {
    pub id: String,
    pub display_name: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
/// Messages sent from the server to peers
pub enum OutgoingMessage {
    /// Confirms registration
    Registered(RegisteredMessage),
    /// Notifies listeners that a producer was registered
    #[serde(rename_all = "camelCase")]
    ProducerAdded {
        peer_id: String,
        display_name: Option<String>,
    },
    /// Notifies listeners that a producer was removed
    #[serde(rename_all = "camelCase")]
    ProducerRemoved {
        peer_id: String,
        display_name: Option<String>,
    },
    /// Instructs a peer to generate an offer
    #[serde(rename_all = "camelCase")]
    StartSession { peer_id: String },
    /// Signals that the session the peer was in was ended
    #[serde(rename_all = "camelCase")]
    EndSession { peer_id: String },
    /// Messages directly forwarded from one peer to another
    Peer(PeerMessage),
    /// Provides the current list of consumer peers
    List { producers: Vec<Peer> },
    /// Notifies that an error occurred with the peer's current session
    Error { details: String },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "peerType")]
#[serde(rename_all = "camelCase")]
/// Register with a peer type
pub enum RegisterMessage {
    /// Register as a producer
    #[serde(rename_all = "camelCase")]
    Producer {
        #[serde(default)]
        display_name: Option<String>,
    },
    /// Register as a consumer
    #[serde(rename_all = "camelCase")]
    Consumer {
        #[serde(default)]
        display_name: Option<String>,
    },
    /// Register as a listener
    #[serde(rename_all = "camelCase")]
    Listener {
        #[serde(default)]
        display_name: Option<String>,
    },
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
/// Ask the server to start a session with a producer peer
pub struct StartSessionMessage {
    /// Identifies the peer
    pub peer_id: String,
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
/// Conveys a SDP
pub enum SdpMessage {
    /// Conveys an offer
    Offer {
        /// The SDP
        sdp: String,
    },
    /// Conveys an answer
    Answer {
        /// The SDP
        sdp: String,
    },
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
/// Contents of the peer message
pub enum PeerMessageInner {
    /// Conveys an ICE candidate
    #[serde(rename_all = "camelCase")]
    Ice {
        /// The candidate string
        candidate: String,
        /// The mline index the candidate applies to
        sdp_m_line_index: u32,
    },
    Sdp(SdpMessage),
}

#[derive(Serialize, Deserialize, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
/// Messages directly forwarded from one peer to another
pub struct PeerMessage {
    /// The identifier of the peer, which must be in a session with the sender
    pub peer_id: String,
    /// The contents of the message
    #[serde(flatten)]
    pub peer_message: PeerMessageInner,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(rename_all = "camelCase")]
/// End a session
pub struct EndSessionMessage {
    /// The identifier of the peer to end the session with
    pub peer_id: String,
}

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
#[serde(rename_all = "camelCase")]
/// Messages received by the server from peers
pub enum IncomingMessage {
    /// Register as a peer type
    Register(RegisterMessage),
    /// Start a session with a producer peer
    StartSession(StartSessionMessage),
    /// End an existing session
    EndSession(EndSessionMessage),
    /// Send a message to a peer the sender is currently in session with
    Peer(PeerMessage),
    /// Retrieve the current list of producers
    List,
}
