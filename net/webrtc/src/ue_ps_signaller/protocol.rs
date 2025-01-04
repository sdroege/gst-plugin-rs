use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug)]
#[serde(tag = "type")]
pub enum Message {
    #[serde(rename = "config")]
    Config(Config),
    #[serde(rename = "identify")]
    Identify(Identify),
    #[serde(rename = "endpointId")]
    EndpointId(EndpointId),
    #[serde(rename = "endpointIdConfirm")]
    EndpointIdConfirm(EndpointIdConfirm),
    #[serde(rename = "streamerIdChanged")]
    StreamerIdChanged(StreamerIdChanged),
    #[serde(rename = "listStreamers")]
    ListStreamers(ListStreamers),
    #[serde(rename = "streamerList")]
    StreamerList(StreamerList),
    #[serde(rename = "subscribe")]
    Subscribe(Subscribe),
    #[serde(rename = "unsubscribe")]
    Unsubscribe(Unsubscribe),
    #[serde(rename = "playerConnected")]
    PlayerConnected(PlayerConnected),
    #[serde(rename = "playerDisconnected")]
    PlayerDisconnected(PlayerDisconnected),
    #[serde(rename = "offer")]
    Offer(Offer),
    #[serde(rename = "answer")]
    Answer(Answer),
    #[serde(rename = "iceCandidate")]
    IceCandidate(IceCandidate),
    #[serde(rename = "disconnectPlayer")]
    DisconnectPlayer(DisconnectPlayer),
    #[serde(rename = "ping")]
    Ping(Ping),
    #[serde(rename = "pong")]
    Pong(Pong),
    #[serde(rename = "streamerDisconnected")]
    StreamerDisconnected(StreamerDisconnected),
    #[serde(rename = "layerPreference")]
    LayerPreference(LayerPreference),
    #[serde(rename = "dataChannelRequest")]
    DataChannelRequest(DataChannelRequest),
    #[serde(rename = "peerDataChannels")]
    PeerDataChannels(PeerDataChannels),
    #[serde(rename = "peerDataChannelsReady")]
    PeerDataChannelsReady(PeerDataChannelsReady),
    #[serde(rename = "streamerDataChannels")]
    StreamerDataChannels(StreamerDataChannels),
    #[serde(rename = "startStreaming")]
    StartStreaming(StartStreaming),
    #[serde(rename = "stopStreaming")]
    StopStreaming(StopStreaming),
    #[serde(rename = "playerCount")]
    PlayerCount(PlayerCount),
    #[serde(rename = "stats")]
    Stats(Stats),
}

/// *
/// This is a user defined structure that is sent as part of the `config`
/// message. Left empty here because everything is optional.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PeerConnectionOptions {}
/// *
/// A config message is sent to each connecting peer when it connects to
/// describe to them the setup of the signalling server they're
/// connecting to.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// The user defined peer connection options
    pub peer_connection_options: Option<PeerConnectionOptions>,
    /// The signalling protocol version the signalling server is using
    pub protocol_version: Option<String>,
}

/// *
/// A request for a new streamer to give itself an ID. The flow for these
/// messages should be connect->identify->endpointId->endpointIdConfirm
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Identify {}

/// *
/// Message is consumed by the Signalling Server. Specifies an id for the
/// streamer. This is used to uniquely identify multiple streamers connected
/// to the same Signalling Server.
/// Note: to preserve backward compatibility when Streamer IDs were optional,
/// when a Streamer first connects it is assigned a temporary ID which
/// allows use of older Streamers if needed.
/// Note: Streamer IDs must be unique and so if the ID provided here clashes
/// with an existing ID, the ID may be altered slightly (usually just an
/// appended number). The streamer will be sent an `endpointIdConfirm`
/// message to notify it of it's final ID.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EndpointId {
    /// The requested ID of the streamer.
    pub id: String,
    /// The signalling protocol version the streamer is using
    pub protocol_version: Option<String>,
}

/// *
/// A response to `endpointId` that will notify the streamer of its final
/// ID given. Since streamer IDs must be unique the requested ID may not be
/// available and may need to be altered.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct EndpointIdConfirm {
    /// The final ID of the streamer.
    pub committed_id: String,
}
/// *
/// Message is used to communicate to players that the streamer it is currently
/// subscribed to is changing its ID. This allows players to keep track of it's
/// currently subscribed streamer and allow auto reconnects to the correct
/// streamer. This happens if a streamer sends an `endpointID` message after it
/// already has an ID assigned. (Can happen if it is late to respond to the
/// `identify` message and is auto assigned a legacy ID.)
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StreamerIdChanged {
    /// The new ID of the streamer.
    pub new_id: String,
}
/// *
/// A request to the signalling server to send the player a list of
/// available streamers it could possibly subscribe to.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ListStreamers {}
/// *
/// Message is a reply to `listStreamers` from a player. Replies with a list of
/// currently active streamers connected to this server.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StreamerList {
    /// A list of streamer IDs active on the server.
    pub ids: Vec<String>,
}
/// *
/// Message is consumed by the signalling server. Tells the signalling server
/// that the player requests to subscribe to the given stream.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Subscribe {
    /// The ID of the streamer the player wishes to subscribe to.
    pub streamer_id: String,
}
/// *
/// Message is consumed by the signalling server. Tells the signalling server
/// that the player wishes to unsubscribe from the current stream. The player
/// must have previously used the `subscribe` message for this to have any effect.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Unsubscribe {}
/// *
/// A message sent to a streamer to notify it that a player has just
/// subscribed to it.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PlayerConnected {
    /// True if the player should be given a datachannel for stream control purposes.
    pub data_channel: bool,
    /// True if the player connected is an SFU
    pub sfu: bool,
    /// The ID of the player that connected.
    pub player_id: String,
}
/// *
/// Message is used to notify a streamer that a player has
/// unsubscribed/disconnected from the stream.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PlayerDisconnected {
    /// The ID of the player that disconnected.
    pub player_id: String,
}
/// *
/// An offer message can be an offer of a WebRTC stream, or an offer to
/// receive a WebRTC stream, depending on the configuration of the player.
/// The default behaviour is that when a player subscribes to a streamer
/// the streamer will offer the stream to the new player.
/// An alternative configuration exists where a player can be configured
/// to offer to receive and in that case the player will subscribe to a
/// streamer and then offer to receive the stream.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Offer {
    /// The SDP payload from WebRTC
    pub sdp: String,
    /// If being sent to a player this should be a valid player ID
    pub player_id: Option<String>,
    /// Indicates that this offer is coming from an SFU.
    pub sfu: Option<bool>,
}
/// *
/// This is a response to an `offer` message. It contains the answer `SDP`.
/// Part of the normal subscribe flow. A peer will subscribe to a streamer
/// and depending on whether `offer_to_receive` is set, one peer will make
/// an offer and the other should answer.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Answer {
    /// The WebRTC SDP payload
    pub sdp: String,
    /// If being sent to a player this should be set to a valid player ID.
    pub player_id: Option<String>,
}
/// *
/// A submessage that contains data from a WebRTC ICE candidate.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct IceCandidateData {
    pub candidate: String,
    pub sdp_mid: String,
    pub sdp_m_line_index: i32,
    pub username_fragment: Option<String>,
}
/// *
/// A single ICE candidate entry from WebRTC. Notifies a peer of a possible
/// connection option to another peer.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct IceCandidate {
    /// The ICE candidate data from WebRTC
    pub candidate: Option<IceCandidateData>,
    /// If being sent to a player this should be a valid player ID.
    pub player_id: Option<String>,
}
/// *
/// Message is consumed by the Signalling Server. Requests that the
/// signalling server disconnect the given player matching the player ID.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DisconnectPlayer {
    /// The ID of the player to disconnect.
    pub player_id: String,
    /// An optional reason string to send to the player.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
/// *
/// A keepalive ping message used to test that the connection is still open.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Ping {
    /// The current time
    pub time: i32,
}
/// *
/// Message is a reply to `ping` from a streamer. Replies with the time from the
/// ping message.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Pong {
    /// The echoed time from the ping message
    pub time: i32,
}
/// *
/// Message is used to notify players when a Streamer disconnects from the
/// signalling server.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StreamerDisconnected {}
/// *
/// Message is forwarded to a connected SFU. Sends a preferred layer index to a
/// connected SFU for a specified player. Useful for switching between SFU
/// quality layers to force a certain resolution/quality option either as part
/// of UX or testing.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct LayerPreference {
    /// The requested spatial layer
    pub spatial_layer: i32,
    /// The requested temporal layer
    pub temporal_layer: i32,
    /// The player ID this preference refers to
    pub player_id: String,
}
/// *
/// Message is forwarded to a connected SFU. Tells the SFU that the player
/// requests data channels to the streamer.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct DataChannelRequest {}
/// *
/// Message is forwarded to a player. Sends information to the player about what
/// data channels to use for sending/receiving with the streamer.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PeerDataChannels {
    /// The player ID this message refers to.
    pub player_id: String,
    /// The channel ID to use for sending data.
    pub send_stream_id: i32,
    /// The channel ID to use for receiving data.
    pub recv_stream_id: i32,
}
/// *
/// Message is forwarded to a connected SFU. Tells the SFU that the player is
/// ready for data channels to be negotiated.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PeerDataChannelsReady {}
/// *
/// Message is forwarded to the streamer. Sends a request to the streamer to
/// open up data channels for a given player.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StreamerDataChannels {
    /// The SFU the player is connected to
    pub sfu_id: String,
    /// The channel ID to use for sending data.
    pub send_stream_id: i32,
    /// The channel ID to use for receiving data.
    pub recv_stream_id: i32,
}
/// *
/// Sent by the SFU to indicate that it is now streaming.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StartStreaming {}
/// *
/// Sent by the SFU to indicate that it is now no longer streaming.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct StopStreaming {}
/// *
/// DEPRECATED Message is sent to players to indicate how many currently connected players
/// there are on this signalling server. (Note: This is mostly old behaviour and
/// is not influenced by multi streamers or who is subscribed to what streamer.
/// It just reports the number of players it knows about.)
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct PlayerCount {
    /// The number of players connected.
    pub count: i32,
}
/// *
/// DEPRECATED Message is consumed by the signalling server. Will print out the provided
/// stats data on the console.
#[derive(serde::Serialize, serde::Deserialize)]
#[allow(clippy::derive_partial_eq_without_eq)]
#[derive(Clone, PartialEq, Debug)]
#[serde(rename_all = "camelCase")]
pub struct Stats {
    /// The stats data to echo.
    pub data: String,
}
