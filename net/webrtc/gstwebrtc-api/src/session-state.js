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

/**
 * Session states enumeration.<br>
 * Session state always increases from idle to closed and never switches backwards.
 * @typedef {enum} GstWebRTCAPI.SessionState
 * @readonly
 * @property {0} idle - Default state when creating a new session, goes to <i>connecting</i> when starting
 * the session.
 * @property {1} connecting - Session is trying to connect to remote peers, goes to <i>streaming</i> in case of
 * success or <i>closed</i> in case of error.
 * @property {2} streaming - Session is correctly connected to remote peers and currently streaming audio/video, goes
 * to <i>closed</i> when any peer closes the session.
 * @property {3} closed - Session is closed and can be garbage collected, state will not change anymore.
 */
const SessionState = Object.freeze({
  idle: 0,
  connecting: 1,
  streaming: 2,
  closed: 3
});

export { SessionState as default };
