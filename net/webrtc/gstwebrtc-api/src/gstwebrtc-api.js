/*
 * gstwebrtc-api
 *
 * Copyright (C) 2022 Igalia S.L. <info@igalia.com>
 *   Author: Loïc Le Page <llepage@igalia.com>
 *
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/.
 */

import defaultConfig from "./config";
import ComChannel from "./com-channel";
import SessionState from "./session-state";

const apiState = {
  config: null,
  channel: null,
  producers: {},
  connectionListeners: [],
  producersListeners: []
};

/**
 * @interface gstWebRTCAPI.ConnectionListener
 */
/**
 * Callback method called when this client connects to the WebRTC signaling server.
 * The callback implementation should not throw any exception.
 * @method gstWebRTCAPI.ConnectionListener#connected
 * @abstract
 * @param {string} clientId - The unique identifier of this WebRTC client.<br>This identifier is provided by the
 * signaling server to uniquely identify each connected peer.
 */
/**
 * Callback method called when this client disconnects from the WebRTC signaling server.
 * The callback implementation should not throw any exception.
 * @method gstWebRTCAPI.ConnectionListener#disconnected
 * @abstract
 */

/**
 * Registers a connection listener that will be called each time the WebRTC API connects to or disconnects from the
 * signaling server.
 * @function
 * @memberof gstWebRTCAPI
 * @instance
 * @param {gstWebRTCAPI.ConnectionListener} listener - The connection listener to register.
 * @returns {boolean} true in case of success (or if the listener was already registered), or false if the listener
 * doesn't implement all callback functions and cannot be registered.
 */
function registerConnectionListener(listener) {
  if (!listener || (typeof (listener) !== "object") ||
        (typeof (listener.connected) !== "function") ||
        (typeof (listener.disconnected) !== "function")) {
    return false;
  }

  if (!apiState.connectionListeners.includes(listener)) {
    apiState.connectionListeners.push(listener);
  }

  return true;
}

/**
 * Unregisters a connection listener.<br>
 * The removed listener will never be called again and can be garbage collected.
 * @function
 * @memberof gstWebRTCAPI
 * @instance
 * @param {gstWebRTCAPI.ConnectionListener} listener - The connection listener to unregister.
 * @returns {boolean} true if the listener is found and unregistered, or false if the listener was not previously
 * registered.
 */
function unregisterConnectionListener(listener) {
  const idx = apiState.connectionListeners.indexOf(listener);
  if (idx >= 0) {
    apiState.connectionListeners.splice(idx, 1);
    return true;
  }

  return false;
}

/**
 * Unregisters all previously registered connection listeners.
 * @function
 * @memberof gstWebRTCAPI
 * @instance
 */
function unregisterAllConnectionListeners() {
  apiState.connectionListeners = [];
}

/**
 * Creates a new producer session.
 * <p>You can only create a producer session at once.<br>
 * To request streaming from a new stream you will first need to close the previous producer session.</p>
 * <p>You can only request a producer session while you are connected to the signaling server. You can use the
 * {@link gstWebRTCAPI.ConnectionListener} interface and {@link gstWebRTCAPI#registerConnectionListener} function to
 * listen to the connection state.</p>
 * @function
 * @memberof gstWebRTCAPI
 * @instance
 * @param {external:MediaStream} stream - The audio/video stream to offer as a producer through WebRTC.
 * @returns {gstWebRTCAPI.ProducerSession} The created producer session or null in case of error. To start streaming,
 * you still need to call {@link gstWebRTCAPI.ProducerSession#start} after adding on the returned session all the event
 * listeners you may need.
 */
function createProducerSession(stream) {
  if (apiState.channel) {
    return apiState.channel.createProducerSession(stream);
  } else {
    return null;
  }
}

/**
 * Information about a remote producer registered by the signaling server.
 * @typedef {object} gstWebRTCAPI.Producer
 * @readonly
 * @property {string} id - The remote producer unique identifier set by the signaling server (always non-empty).
 * @property {object} meta - Free-form object containing extra information about the remote producer (always non-null,
 * but may be empty). Its content depends on your application.
 */

/**
 * Gets the list of all remote WebRTC producers available on the signaling server.
 * <p>The remote producers list is only populated once you've connected to the signaling server. You can use the
 * {@link gstWebRTCAPI.ConnectionListener} interface and {@link gstWebRTCAPI#registerConnectionListener} function to
 * listen to the connection state.</p>
 * @function
 * @memberof gstWebRTCAPI
 * @instance
 * @returns {gstWebRTCAPI.Producer[]} The list of remote WebRTC producers available.
 */
function getAvailableProducers() {
  return Object.values(apiState.producers);
}

/**
 * @interface gstWebRTCAPI.ProducersListener
 */
/**
 * Callback method called when a remote producer is added on the signaling server.
 * The callback implementation should not throw any exception.
 * @method gstWebRTCAPI.ProducersListener#producerAdded
 * @abstract
 * @param {gstWebRTCAPI.Producer} producer - The remote producer added on server-side.
 */
/**
 * Callback method called when a remote producer is removed from the signaling server.
 * The callback implementation should not throw any exception.
 * @method gstWebRTCAPI.ProducersListener#producerRemoved
 * @abstract
 * @param {gstWebRTCAPI.Producer} producer - The remote producer removed on server-side.
 */

/**
 * Registers a producers listener that will be called each time a producer is added or removed on the signaling
 * server.
 * @function
 * @memberof gstWebRTCAPI
 * @instance
 * @param {gstWebRTCAPI.ProducersListener} listener - The producer listener to register.
 * @returns {boolean} true in case of success (or if the listener was already registered), or false if the listener
 * doesn't implement all callback functions and cannot be registered.
 */
function registerProducersListener(listener) {
  if (!listener || (typeof (listener) !== "object") ||
        (typeof (listener.producerAdded) !== "function") ||
        (typeof (listener.producerRemoved) !== "function")) {
    return false;
  }

  if (!apiState.producersListeners.includes(listener)) {
    apiState.producersListeners.push(listener);
  }

  return true;
}

/**
 * Unregisters a producers listener.<br>
 * The removed listener will never be called again and can be garbage collected.
 * @function
 * @memberof gstWebRTCAPI
 * @instance
 * @param {gstWebRTCAPI.ProducersListener} listener - The producers listener to unregister.
 * @returns {boolean} true if the listener is found and unregistered, or false if the listener was not previously
 * registered.
 */
function unregisterProducersListener(listener) {
  const idx = apiState.producersListeners.indexOf(listener);
  if (idx >= 0) {
    apiState.producersListeners.splice(idx, 1);
    return true;
  }

  return false;
}

/**
 * Unregisters all previously registered producers listeners.
 * @function
 * @memberof gstWebRTCAPI
 * @instance
 */
function unregisterAllProducersListeners() {
  apiState.producersListeners = [];
}

/**
 * Creates a consumer session by connecting the local client to a remote WebRTC producer.
 * <p>You can only create one consumer session per remote producer.</p>
 * <p>You can only request a new consumer session while you are connected to the signaling server. You can use the
 * {@link gstWebRTCAPI.ConnectionListener} interface and {@link gstWebRTCAPI#registerConnectionListener} function to
 * listen to the connection state.</p>
 * @function
 * @memberof gstWebRTCAPI
 * @instance
 * @param {string} producerId - The unique identifier of the remote producer to connect to.
 * @returns {gstWebRTCAPI.ConsumerSession} The WebRTC session between the selected remote producer and this local
 * consumer, or null in case of error. To start connecting and receiving the remote streams, you still need to call
 * {@link gstWebRTCAPI.ConsumerSession#connect} after adding on the returned session all the event listeners you may
 * need.
 */
function createConsumerSession(producerId) {
  if (apiState.channel) {
    return apiState.channel.createConsumerSession(producerId);
  } else {
    return null;
  }
}

/**
 * The GStreamer WebRTC Javascript API.
 * @namespace gstWebRTCAPI
 */
const gstWebRTCAPI = Object.freeze({
  SessionState: SessionState,
  registerConnectionListener: registerConnectionListener,
  unregisterConnectionListener: unregisterConnectionListener,
  unregisterAllConnectionListeners: unregisterAllConnectionListeners,
  createProducerSession: createProducerSession,
  getAvailableProducers: getAvailableProducers,
  registerProducersListener: registerProducersListener,
  unregisterProducersListener: unregisterProducersListener,
  unregisterAllProducersListeners: unregisterAllProducersListeners,
  createConsumerSession: createConsumerSession
});

function triggerConnected(clientId) {
  for (const listener of apiState.connectionListeners) {
    try {
      listener.connected(clientId);
    } catch (ex) {
      console.error("a listener callback should not throw any exception", ex);
    }
  }
}

function triggerDisconnected() {
  for (const listener of apiState.connectionListeners) {
    try {
      listener.disconnected();
    } catch (ex) {
      console.error("a listener callback should not throw any exception", ex);
    }
  }
}

function triggerProducerAdded(producer) {
  if (producer.id in apiState.producers) {
    return;
  }

  apiState.producers[producer.id] = producer;
  for (const listener of apiState.producersListeners) {
    try {
      listener.producerAdded(producer);
    } catch (ex) {
      console.error("a listener callback should not throw any exception", ex);
    }
  }
}

function triggerProducerRemoved(producerId) {
  if (producerId in apiState.producers) {
    const producer = apiState.producers[producerId];
    delete apiState.producers[producerId];

    for (const listener of apiState.producersListeners) {
      try {
        listener.producerRemoved(producer);
      } catch (ex) {
        console.error("a listener callback should not throw any exception", ex);
      }
    }
  }
}

function connectChannel() {
  if (apiState.channel) {
    const oldChannel = apiState.channel;
    apiState.channel = null;
    oldChannel.close();
    for (const key in apiState.producers) {
      triggerProducerRemoved(key);
    }
    apiState.producers = {};
    triggerDisconnected();
  }

  apiState.channel = new ComChannel(
    apiState.config.signalingServerUrl,
    apiState.config.meta,
    apiState.config.webrtcConfig);

  apiState.channel.addEventListener("error", (event) => {
    if (event.target === apiState.channel) {
      console.error(event.message, event.error);
    }
  });

  apiState.channel.addEventListener("closed", (event) => {
    if (event.target === apiState.channel) {
      apiState.channel = null;
      for (const key in apiState.producers) {
        triggerProducerRemoved(key);
      }
      apiState.producers = {};
      triggerDisconnected();

      if (apiState.config.reconnectionTimeout > 0) {
        window.setTimeout(connectChannel, apiState.config.reconnectionTimeout);
      }
    }
  });

  apiState.channel.addEventListener("ready", (event) => {
    if (event.target === apiState.channel) {
      triggerConnected(apiState.channel.channelId);
    }
  });

  apiState.channel.addEventListener("producerAdded", (event) => {
    if (event.target === apiState.channel) {
      triggerProducerAdded(event.detail);
    }
  });

  apiState.channel.addEventListener("producerRemoved", (event) => {
    if (event.target === apiState.channel) {
      triggerProducerRemoved(event.detail.id);
    }
  });
}

function start(userConfig) {
  if (apiState.config) {
    throw new Error("GstWebRTC API is already started");
  }

  const config = Object.assign({}, defaultConfig);
  if (userConfig && (typeof (userConfig) === "object")) {
    Object.assign(config, userConfig);
  }

  if (typeof (config.meta) !== "object") {
    config.meta = null;
  }

  apiState.config = config;
  connectChannel();
}

export { gstWebRTCAPI, start };
