/* vim: set sts=4 sw=4 et :
 *
 * Demo Javascript app for negotiating and streaming a sendrecv webrtc stream
 * with a GStreamer app. Runs only in passive mode, i.e., responds to offers
 * with answers, exchanges ICE candidates, and streams.
 *
 * Author: Nirbheek Chauhan <nirbheek@centricular.com>
 */

// Set this to override the automatic detection in websocketServerConnect()
var ws_server;
var ws_port;
// Override with your own STUN servers if you want
var rtc_configuration = {iceServers: [{urls: "stun:stun.l.google.com:19302"},
                                       /* TODO: do not keep these static and in clear text in production,
                                        * and instead use one of the mechanisms discussed in
                                        * https://groups.google.com/forum/#!topic/discuss-webrtc/nn8b6UboqRA
                                        */
                                      {'urls': 'turn:turn.homeneural.net:3478?transport=udp',
                                       'credential': '1qaz2wsx',
                                       'username': 'test'
                                      }],
                         /* Uncomment the following line to ensure the turn server is used
                          * while testing. This should be kept commented out in production,
                          * as non-relay ice candidates should be preferred
                          */
                         // iceTransportPolicy: "relay",
                        };

var sessions = {}

/* https://stackoverflow.com/questions/105034/create-guid-uuid-in-javascript */
function getOurId() {
  return 'xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx'.replace(/[xy]/g, function(c) {
    var r = Math.random() * 16 | 0, v = c == 'x' ? r : (r & 0x3 | 0x8);
    return v.toString(16);
  });
}

function Uint8ToString(u8a){
  var CHUNK_SZ = 0x8000;
  var c = [];
  for (var i=0; i < u8a.length; i+=CHUNK_SZ) {
    c.push(String.fromCharCode.apply(null, u8a.subarray(i, i+CHUNK_SZ)));
  }
  return c.join("");
}

function Session(our_id, peer_id, closed_callback) {
    this.peer_connection = null;
    this.ws_conn = null;
    this.peer_id = peer_id;
    this.our_id = our_id;
    this.closed_callback = closed_callback;
    this.data_channel = null;
    this.input = null;

    this.getVideoElement = function() {
        return document.getElementById("stream-" + this.our_id);
    };

    this.resetState = function() {
        if (this.peer_connection) {
            this.peer_connection.close();
            this.peer_connection = null;
        }
        var videoElement = this.getVideoElement();
        if (videoElement) {
            videoElement.pause();
            videoElement.src = "";
        }

        var session_div = document.getElementById("session-" + this.our_id);
        if (session_div) {
            session_div.parentNode.removeChild(session_div);
        }
        if (this.ws_conn) {
            this.ws_conn.close();
            this.ws_conn = null;
        }

        this.input && this.input.detach();
        this.data_channel = null;
    };

    this.handleIncomingError = function(error) {
        this.resetState();
        this.closed_callback(this.our_id);
    };

    this.setStatus = function(text) {
        console.log(text);
        var span = document.getElementById("status-" + this.our_id);
        // Don't set the status if it already contains an error
        if (!span.classList.contains('error'))
            span.textContent = text;
    };

    this.setError = function(text) {
        console.error(text);
        var span = document.getElementById("status-" + this.our_id);
        span.textContent = text;
        span.classList.add('error');
        this.resetState();
        this.closed_callback(this.our_id);
    };

    // Local description was set, send it to peer
    this.onLocalDescription = function(desc) {
        console.log("Got local description: " + JSON.stringify(desc), this);
        var thiz = this;
        this.peer_connection.setLocalDescription(desc).then(() => {
            this.setStatus("Sending SDP answer");
            var sdp = {'peer-id': this.peer_id, 'sdp': this.peer_connection.localDescription.toJSON()}
            this.ws_conn.send(JSON.stringify(sdp));
        }).catch(function(e) {
            thiz.setError(e);
        });
    };

    this.onRemoteDescriptionSet = function() {
        this.setStatus("Remote SDP set");
        this.setStatus("Got SDP offer");
        this.peer_connection.createAnswer()
        .then(this.onLocalDescription.bind(this)).catch(this.setError);
    }

    // SDP offer received from peer, set remote description and create an answer
    this.onIncomingSDP = function(sdp) {
        var thiz = this;
        this.peer_connection.setRemoteDescription(sdp)
            .then(this.onRemoteDescriptionSet.bind(this))
            .catch(function(e) {
                thiz.setError(e)
            });
    };

    // ICE candidate received from peer, add it to the peer connection
    this.onIncomingICE = function(ice) {
        var candidate = new RTCIceCandidate(ice);
        var thiz = this;
        this.peer_connection.addIceCandidate(candidate).catch(function(e) {
            thiz.setError(e)
        });
    };

    this.onServerMessage = function(event) {
        console.log("Received " + event.data);
        if (event.data.startsWith("REGISTERED")) {
            this.setStatus("Registered with server");
            this.connectPeer();
        } else if (event.data.startsWith("ERROR")) {
            this.handleIncomingError(event.data);
            return;
        } else {
            // Handle incoming JSON SDP and ICE messages
            try {
                msg = JSON.parse(event.data);
            } catch (e) {
                if (e instanceof SyntaxError) {
                    this.handleIncomingError("Error parsing incoming JSON: " + event.data);
                } else {
                    this.handleIncomingError("Unknown error parsing response: " + event.data);
                }
                return;
            }

            // Incoming JSON signals the beginning of a call
            if (!this.peer_connection)
                this.createCall(msg);

            if (msg.sdp != null) {
                this.onIncomingSDP(msg.sdp);
            } else if (msg.ice != null) {
                this.onIncomingICE(msg.ice);
            } else {
                this.handleIncomingError("Unknown incoming JSON: " + msg);
            }
        }
    };

    this.streamIsPlaying = function(e) {
        this.setStatus("Streaming");
    };

    this.onServerClose = function(event) {
        this.resetState();
        this.closed_callback(this.our_id);
    };

    this.onServerError = function(event) {
        this.handleIncomingError('Server error');
    };

    this.websocketServerConnect = function() {
        // Clear errors in the status span
        var span = document.getElementById("status-" + this.our_id);
        span.classList.remove('error');
        span.textContent = '';
        console.log("Our ID:", this.our_id);
        var ws_port = ws_port || '8443';
        if (window.location.protocol.startsWith ("file")) {
            var ws_server = ws_server || "127.0.0.1";
        } else if (window.location.protocol.startsWith ("http")) {
            var ws_server = ws_server || window.location.hostname;
        } else {
            throw new Error ("Don't know how to connect to the signalling server with uri" + window.location);
        }
        var ws_url = 'ws://' + ws_server + ':' + ws_port
        this.setStatus("Connecting to server " + ws_url);
        this.ws_conn = new WebSocket(ws_url);
        /* When connected, immediately register with the server */
        this.ws_conn.addEventListener('open', (event) => {
            this.ws_conn.send('REGISTER CONSUMER');
            this.setStatus("Registering with server");
        });
        this.ws_conn.addEventListener('error', this.onServerError.bind(this));
        this.ws_conn.addEventListener('message', this.onServerMessage.bind(this));
        this.ws_conn.addEventListener('close', this.onServerClose.bind(this));
    };

    this.connectPeer = function() {
        this.setStatus("Connecting " + this.peer_id);

        this.ws_conn.send("START_SESSION " + this.peer_id);
    };

    this.onRemoteStreamAdded = function(event) {
        var videoTracks = event.stream.getVideoTracks();
        var audioTracks = event.stream.getAudioTracks();

        if (videoTracks.length > 0) {
            console.log('Incoming stream: ' + videoTracks.length + ' video tracks and ' + audioTracks.length + ' audio tracks');
            this.getVideoElement().srcObject = event.stream;
            this.getVideoElement().play();
        } else {
            this.handleIncomingError('Stream with unknown tracks added, resetting');
        }
    };

    this.createCall = function(msg) {
        console.log('Creating RTCPeerConnection');

        this.peer_connection = new RTCPeerConnection(rtc_configuration);
        this.peer_connection.onaddstream = this.onRemoteStreamAdded.bind(this);

        this.peer_connection.ondatachannel = (event) => {
            console.log(`Data channel created: ${event.channel.label}`);
            this.data_channel = event.channel;

            video_element = this.getVideoElement();
            if (video_element) {
                this.input = new Input(video_element, (data) => {
                    if (this.data_channel) {
                        console.log(`Navigation data: ${data}`);
                        this.data_channel.send(data);
                    }
                });
            }

            this.data_channel.onopen = (event) => {
                console.log("Receive channel opened, attaching input");
                this.input.attach();
            }
            this.data_channel.onclose = (event) => {
                console.info("Receive channel closed");
                this.input && this.input.detach();
                this.data_channel = null;
            }
            this.data_channel.onerror = (event) => {
                this.input && this.input.detach();
                console.warn("Error on receive channel", event.data);
                this.data_channel = null;
            }

            let buffer = [];
            this.data_channel.onmessage = (event) => {
                if (typeof event.data === 'string' || event.data instanceof String) {
                    if (event.data == 'BEGIN_IMAGE')
                        buffer = [];
                    else if (event.data == 'END_IMAGE') {
                        var decoder = new TextDecoder("ascii");
                        var array_buffer = new Uint8Array(buffer);
                        var str = decoder.decode(array_buffer);
                        let img = document.getElementById("image");
                        img.src = 'data:image/png;base64, ' + str;
                    }
                } else {
                    var i, len = buffer.length
                    var view = new DataView(event.data);
                    for (i = 0; i < view.byteLength; i++) {
                        buffer[len + i] = view.getUint8(i);
                    }
                }
            }
        }

        if (!msg.sdp) {
            console.log("WARNING: First message wasn't an SDP message!?");
        }

        this.peer_connection.onicecandidate = (event) => {
            if (event.candidate == null) {
                console.log("ICE Candidate was null, done");
                return;
            }
            this.ws_conn.send(JSON.stringify({'ice': event.candidate.toJSON(), 'peer-id': this.peer_id}));
        };

        this.setStatus("Created peer connection for call, waiting for SDP");
    };

    document.getElementById("stream-" + this.our_id).addEventListener("playing", this.streamIsPlaying.bind(this), false);

    this.websocketServerConnect();
}

function startSession() {
    var peer_id = document.getElementById("camera-id").value;

    if (peer_id === "") {
        return;
    }

    sessions[peer_id] = new Session(peer_id);
}

function session_closed(peer_id) {
    sessions[peer_id] = null;
}

function addPeer(peer_id) {
    var nav_ul = document.getElementById("camera-list");
    var li_str = '<li id="peer-' + peer_id + '"><button class="button button1">' + peer_id + '</button></li>'

    nav_ul.insertAdjacentHTML('beforeend', li_str);
    var li = document.getElementById("peer-" + peer_id);
    li.onclick = function(e) {
        var sessions_div = document.getElementById('sessions');
        var our_id = getOurId();
        var session_div_str = '<div class="session" id="session-' + our_id + '"><video preload="none" class="stream" id="stream-' + our_id + '"></video><p>Status: <span id="status-' + our_id + '">unknown</span></p></div>'
        sessions_div.insertAdjacentHTML('beforeend', session_div_str);
        sessions[peer_id] = new Session(our_id, peer_id, session_closed);
    }
}

function clearPeers() {
    var nav_ul = document.getElementById("camera-list");

    while (nav_ul.firstChild) {
        nav_ul.removeChild(nav_ul.firstChild);
    }
}

function onServerMessage(event) {
    console.log("Received " + event.data);
    if (event.data.startsWith("REGISTERED ")) {
        console.log("Listener registered");
        ws_conn.send('LIST');
    } else if (event.data.startsWith('LIST')) {
        var split = event.data.split(' ');
        clearPeers();

        for (var i = 1; i < split.length; i++) {
            addPeer(split[i]);
        }
    } else if (event.data.startsWith('ADD PRODUCER')) {
        var split = event.data.split(' ');
        addPeer(split[2]);
    } else if (event.data.startsWith('REMOVE PRODUCER')) {
        var split = event.data.split(' ');
        var li = document.getElementById("peer-" + split[2]);
        li.parentNode.removeChild(li);
    }
};

function clearConnection() {
    ws_conn.removeEventListener('error', onServerError);
    ws_conn.removeEventListener('message', onServerMessage);
    ws_conn.removeEventListener('close', onServerClose);
    ws_conn = null;
}

function onServerClose(event) {
    clearConnection();
    clearPeers();
    console.log("Close");
    window.setTimeout(connect, 1000);
};

function onServerError(event) {
    clearConnection();
    clearPeers();
    console.log("Error", event);
    window.setTimeout(connect, 1000);
};

function connect() {
    var ws_port = ws_port || '8443';
    if (window.location.protocol.startsWith ("file")) {
        var ws_server = ws_server || "127.0.0.1";
    } else if (window.location.protocol.startsWith ("http")) {
        var ws_server = ws_server || window.location.hostname;
    } else {
        throw new Error ("Don't know how to connect to the signalling server with uri" + window.location);
    }
    var ws_url = 'ws://' + ws_server + ':' + ws_port
    console.log("Connecting listener");
    ws_conn = new WebSocket(ws_url);
    ws_conn.addEventListener('open', (event) => {
        ws_conn.send('REGISTER LISTENER');
    });
    ws_conn.addEventListener('error', onServerError);
    ws_conn.addEventListener('message', onServerMessage);
    ws_conn.addEventListener('close', onServerClose);
}

function setup() {
    connect();
}
