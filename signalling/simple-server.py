#!/usr/bin/env python3
#
# Example 1-1 call signalling server
#
# Copyright (C) 2017 Centricular Ltd.
#
#  Author: Nirbheek Chauhan <nirbheek@centricular.com>
#

import os
import sys
import ssl
import logging
import asyncio
import websockets
import argparse
import json
import uuid
import concurrent

from collections import defaultdict

from concurrent.futures._base import TimeoutError

parser = argparse.ArgumentParser(formatter_class=argparse.ArgumentDefaultsHelpFormatter)
parser.add_argument('--addr', default='0.0.0.0', help='Address to listen on')
parser.add_argument('--port', default=8443, type=int, help='Port to listen on')
parser.add_argument('--keepalive-timeout', dest='keepalive_timeout', default=30, type=int, help='Timeout for keepalive (in seconds)')
parser.add_argument('--disable-ssl', default=False, help='Disable ssl', action='store_true')
parser.add_argument('--hide-local-candidates', '-hd', default=False, help='Hide local Ice candidates', action='store_true')

options = parser.parse_args(sys.argv[1:])

ADDR_PORT = (options.addr, options.port)
KEEPALIVE_TIMEOUT = options.keepalive_timeout

logger = logging.getLogger('webrtc.signalling')
handler = logging.StreamHandler(sys.stderr)
formatter = logging.Formatter('%(asctime)s - %(name)s - %(levelname)s - %(message)s')
handler.setFormatter(formatter)
logger.setLevel(logging.INFO)
logger.addHandler(handler)

############### Global data ###############

# Format: {uid: (Peer WebSocketServerProtocol,
#                remote_address,
#                [<'session'>,]}
peers = dict()
# Format: {caller_uid: [callee_uid,],
#          callee_uid: [caller_uid,]}
# Bidirectional mapping between the two peers
sessions = defaultdict(list)

producers = set()
consumers = set()
listeners = set()

############### Helper functions ###############

async def recv_msg_ping(ws, raddr):
    '''
    Wait for a message forever, and send a regular ping to prevent bad routers
    from closing the connection.
    '''
    msg = None
    while msg is None:
        try:
            msg = await asyncio.wait_for(ws.recv(), KEEPALIVE_TIMEOUT)
        except (asyncio.TimeoutError, concurrent.futures._base.TimeoutError):
            logger.debug('Sending keepalive ping to {!r} in recv'.format(raddr))
            await ws.ping()
    return msg

async def cleanup_session(uid):
    for other_id in sessions[uid]:
        await peers[other_id][0].send('END_SESSION {}'.format(uid))
        sessions[other_id].remove(uid)
        logger.info("Cleaned up {} -> {} session".format(uid, other_id))
    del sessions[uid]

async def end_session(uid, other_id):
    if not other_id in sessions[uid]:
        return

    if not uid in sessions[other_id]:
        return

    if not other_id in peers:
        return

    await peers[other_id][0].send('END_SESSION {}'.format(uid))
    sessions[uid].remove(other_id)
    sessions[other_id].remove(uid)

async def remove_peer(uid):
    await cleanup_session(uid)
    if uid in peers:
        ws, raddr, status = peers[uid]
        del peers[uid]
        await ws.close()
        logger.info("Disconnected from peer {!r} at {!r}".format(uid, raddr))

    if uid in producers:
        for peer_id in listeners:
            await peers[peer_id][0].send('REMOVE PRODUCER {}'.format(uid))

    if uid in producers:
        producers.remove(uid)
    elif uid in consumers:
        consumers.remove(uid)
    elif uid in listeners:
        listeners.remove(uid)

############### Handler functions ###############

async def connection_handler(ws, uid):
    global peers, sessions
    raddr = ws.remote_address
    peers_status = []
    peers[uid] = [ws, raddr, peers_status]
    logger.info("Registered peer {!r} at {!r}".format(uid, raddr))
    while True:
        # Receive command, wait forever if necessary
        msg = await recv_msg_ping(ws, raddr)
        # Update current status
        peers_status = peers[uid][2]
        # Requested a session with a specific peer
        if msg.startswith('START_SESSION '):
            logger.info("{!r} command {!r}".format(uid, msg))
            _, callee_id = msg.split(maxsplit=1)
            if callee_id not in peers:
                await ws.send('ERROR peer {!r} not found'.format(callee_id))
                continue
            wsc = peers[callee_id][0]
            await wsc.send('START_SESSION {}'.format(uid))
            logger.info('Session from {!r} ({!r}) to {!r} ({!r})'
                  ''.format(uid, raddr, callee_id, wsc.remote_address))
            # Register session
            peers[uid][2] = peer_status = 'session'
            sessions[uid].append(callee_id)
            peers[callee_id][2] = 'session'
            sessions[callee_id] .append(uid)
        elif msg.startswith('END_SESSION '):
            _, peer_id = msg.split(maxsplit=1)
            await end_session(uid, peer_id)
        elif msg.startswith('LIST'):
            answer = ' '.join(['LIST'] + [other_id for other_id in peers if other_id in producers])
            await ws.send(answer)
        # We are in a session, messages must be relayed
        else:
            # We're in a session, route message to connected peer
            msg = json.loads(msg)

            if options.hide_local_candidates:
                candidate = msg.get("ice", {}).get("candidate")
                if candidate:
                    if candidate.split()[4].endswith(".local"):
                        logger.info(f"Ignoring local candidate: {candidate}")
                        continue

            other_id = msg['peer-id']
            try:
                wso, oaddr, status = peers[other_id]
            except KeyError:
                continue

            msg['peer-id'] = uid
            msg = json.dumps(msg)
            logger.debug("Got peer: {} -> {}: {}".format(uid, other_id, msg))
            await wso.send(msg)

async def register_peer(ws):
    '''
    Register peer
    '''
    raddr = ws.remote_address
    msg = await ws.recv()
    cmd, typ = msg.split(maxsplit=1)

    uid = str(uuid.uuid4())

    while uid in peers:
        uid = str(uuid.uuid4())

    if cmd != 'REGISTER':
        await ws.close(code=1002, reason='invalid protocol')
        raise Exception("Invalid registration from {!r}".format(raddr))
    if typ not in ('PRODUCER', 'CONSUMER', 'LISTENER'):
        await ws.close(code=1002, reason='invalid protocol')
        raise Exception("Invalid registration from {!r}".format(raddr))
    # Send back a HELLO
    await ws.send('REGISTERED {}'.format(uid))
    return typ, uid

async def handler(ws, path):
    '''
    All incoming messages are handled here. @path is unused.
    '''
    raddr = ws.remote_address
    logger.info("Connected to {!r}".format(raddr))
    try:
        typ, peer_id = await register_peer(ws)
    except:
        return
    if typ == 'PRODUCER':
        for other_id in listeners:
            await peers[other_id][0].send('ADD PRODUCER {}'.format(peer_id))
        producers.add(peer_id)
    elif typ == 'CONSUMER':
        consumers.add(peer_id)
    elif typ == 'LISTENER':
        listeners.add(peer_id)

    try:
        await connection_handler(ws, peer_id)
    except websockets.ConnectionClosed:
        logger.info("Connection to peer {!r} closed, exiting handler".format(raddr))
    finally:
        await remove_peer(peer_id)

if options.disable_ssl:
    sslctx = None
else:
    # Create an SSL context to be used by the websocket server
    certpath = '.'
    chain_pem = os.path.join(certpath, 'cert.pem')
    key_pem = os.path.join(certpath, 'key.pem')

    sslctx = ssl.create_default_context()
    try:
        sslctx.load_cert_chain(chain_pem, keyfile=key_pem)
    except FileNotFoundError:
        logger.error("Certificates not found, did you run generate_cert.sh?")
        sys.exit(1)
    # FIXME
    sslctx.check_hostname = False
    sslctx.verify_mode = ssl.CERT_NONE

logger.info("Listening on wss://{}:{}".format(*ADDR_PORT))
# Websocket server
wsd = websockets.serve(handler, *ADDR_PORT, ssl=sslctx,
                       # Maximum number of messages that websockets will pop
                       # off the asyncio and OS buffers per connection. See:
                       # https://websockets.readthedocs.io/en/stable/api.html#websockets.protocol.WebSocketCommonProtocol
                       max_queue=16)

ws_logger = logging.getLogger('websockets.server')
ws_logger.setLevel(logging.ERROR)
ws_logger.addHandler(logging.StreamHandler())

asyncio.get_event_loop().run_until_complete(wsd)
asyncio.get_event_loop().run_forever()
