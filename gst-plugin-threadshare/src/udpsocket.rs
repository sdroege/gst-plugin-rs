// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
//
// This library is free software; you can redistribute it and/or
// modify it under the terms of the GNU Library General Public
// License as published by the Free Software Foundation; either
// version 2 of the License, or (at your option) any later version.
//
// This library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the GNU
// Library General Public License for more details.
//
// You should have received a copy of the GNU Library General Public
// License along with this library; if not, write to the
// Free Software Foundation, Inc., 51 Franklin Street, Suite 500,
// Boston, MA 02110-1335, USA.

use std::sync::{Arc, Mutex};
use std::io;

use gst;
use gst::prelude::*;

use futures::{Async, Future, IntoFuture, Poll, Stream};
use futures::task;
use futures::sync::oneshot;
use tokio::net;

use either::Either;

use iocontext::*;

lazy_static!{
    static ref SOCKET_CAT: gst::DebugCategory = gst::DebugCategory::new(
                "ts-socket",
                gst::DebugColorFlags::empty(),
                "Thread-sharing Socket",
            );
}

#[derive(Clone)]
pub struct Socket(Arc<Mutex<SocketInner>>);

#[derive(PartialEq, Eq, Debug)]
enum SocketState {
    Unscheduled,
    Scheduled,
    Running,
    Shutdown,
}

struct SocketInner {
    element: gst::Element,
    state: SocketState,
    socket: net::UdpSocket,
    buffer_pool: gst::BufferPool,
    current_task: Option<task::Task>,
    shutdown_receiver: Option<oneshot::Receiver<()>>,
    clock: Option<gst::Clock>,
    base_time: Option<gst::ClockTime>,
}

impl Socket {
    pub fn new(
        element: &gst::Element,
        socket: net::UdpSocket,
        buffer_pool: gst::BufferPool,
    ) -> Self {
        Socket(Arc::new(Mutex::new(SocketInner {
            element: element.clone(),
            state: SocketState::Unscheduled,
            socket: socket,
            buffer_pool: buffer_pool,
            current_task: None,
            shutdown_receiver: None,
            clock: None,
            base_time: None,
        })))
    }

    pub fn schedule<U, F, G>(&self, io_context: &IOContext, func: F, err_func: G) -> Result<(), ()>
    where
        F: Fn(gst::Buffer) -> U + Send + 'static,
        U: IntoFuture<Item = (), Error = gst::FlowError> + 'static,
        <U as IntoFuture>::Future: Send + 'static,
        G: FnOnce(Either<gst::FlowError, io::Error>) + Send + 'static,
    {
        // Ready->Paused
        //
        // Need to wait for a possible shutdown to finish first
        // spawn() on the reactor, change state to Scheduled
        let stream = SocketStream(self.clone(), None);

        let mut inner = self.0.lock().unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Scheduling socket");
        if inner.state == SocketState::Scheduled {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket already scheduled");
            return Ok(());
        }

        assert_eq!(inner.state, SocketState::Unscheduled);
        inner.state = SocketState::Scheduled;
        if let Err(_) = inner.buffer_pool.set_active(true) {
            gst_error!(SOCKET_CAT, obj: &inner.element, "Failed to activate buffer pool");
            return Err(());
        }

        let (sender, receiver) = oneshot::channel::<()>();
        inner.shutdown_receiver = Some(receiver);

        let element_clone = inner.element.clone();
        io_context.spawn(
            stream
                .for_each(move |buffer| func(buffer).into_future().map_err(Either::Left))
                .then(move |res| {
                    gst_debug!(
                        SOCKET_CAT,
                        obj: &element_clone,
                        "Socket finished: {:?}",
                        res
                    );

                    if let Err(err) = res {
                        err_func(err);
                    }

                    let _ = sender.send(());

                    Ok(())
                }),
        );
        Ok(())
    }

    pub fn unpause(&self, clock: gst::Clock, base_time: gst::ClockTime) {
        // Paused->Playing
        //
        // Change state to Running and signal task
        let mut inner = self.0.lock().unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Unpausing socket");
        if inner.state == SocketState::Running {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket already unpaused");
            return;
        }

        assert_eq!(inner.state, SocketState::Scheduled);
        inner.state = SocketState::Running;
        inner.clock = Some(clock);
        inner.base_time = Some(base_time);

        if let Some(task) = inner.current_task.take() {
            task.notify();
        }
    }

    pub fn pause(&self) {
        // Playing->Paused
        //
        // Change state to Scheduled and signal task

        let mut inner = self.0.lock().unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Pausing socket");
        if inner.state == SocketState::Scheduled {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket already paused");
            return;
        }

        assert_eq!(inner.state, SocketState::Running);
        inner.state = SocketState::Scheduled;
        inner.clock = None;
        inner.base_time = None;

        if let Some(task) = inner.current_task.take() {
            task.notify();
        }
    }

    pub fn shutdown(&self) {
        // Paused->Ready
        //
        // Change state to Shutdown and signal task, wait for our future to be finished
        // Requires scheduled function to be unblocked! Pad must be deactivated before

        let mut inner = self.0.lock().unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Shutting down socket");
        if inner.state == SocketState::Unscheduled {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket already shut down");
            return;
        }

        assert!(inner.state == SocketState::Scheduled || inner.state == SocketState::Running);
        inner.state = SocketState::Shutdown;

        if let Some(task) = inner.current_task.take() {
            task.notify();
        }

        let shutdown_receiver = inner.shutdown_receiver.take().unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Waiting for socket to shut down");
        drop(inner);

        shutdown_receiver.wait().expect("Already shut down");

        let mut inner = self.0.lock().unwrap();
        inner.state = SocketState::Unscheduled;
        let _ = inner.buffer_pool.set_active(false);
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket shut down");
    }
}

impl Drop for SocketInner {
    fn drop(&mut self) {
        assert_eq!(self.state, SocketState::Unscheduled);
    }
}

struct SocketStream(Socket, Option<gst::MappedBuffer<gst::buffer::Writable>>);

impl Stream for SocketStream {
    type Item = gst::Buffer;
    type Error = Either<gst::FlowError, io::Error>;

    fn poll(&mut self) -> Poll<Option<Self::Item>, Self::Error> {
        let mut inner = (self.0).0.lock().unwrap();
        if inner.state == SocketState::Shutdown {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket shutting down");
            return Ok(Async::Ready(None));
        } else if inner.state == SocketState::Scheduled {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket not running");
            inner.current_task = Some(task::current());
            return Ok(Async::NotReady);
        }

        assert_eq!(inner.state, SocketState::Running);

        gst_debug!(SOCKET_CAT, obj: &inner.element, "Trying to read data");
        let (len, time) = {
            let mut buffer = match self.1 {
                Some(ref mut buffer) => buffer,
                None => match inner.buffer_pool.acquire_buffer(None) {
                    Ok(buffer) => {
                        self.1 = Some(buffer.into_mapped_buffer_writable().unwrap());
                        self.1.as_mut().unwrap()
                    }
                    Err(err) => {
                        gst_debug!(SOCKET_CAT, obj: &inner.element, "Failed to acquire buffer {:?}", err);
                        return Err(Either::Left(err.into_result().unwrap_err()));
                    }
                },
            };

            match inner.socket.poll_recv(buffer.as_mut_slice()) {
                Ok(Async::NotReady) => {
                    gst_debug!(SOCKET_CAT, obj: &inner.element, "No data available");
                    inner.current_task = Some(task::current());
                    return Ok(Async::NotReady);
                }
                Err(err) => {
                    gst_debug!(SOCKET_CAT, obj: &inner.element, "Read error {:?}", err);
                    return Err(Either::Right(err));
                }
                Ok(Async::Ready(len)) => {
                    let time = inner.clock.as_ref().unwrap().get_time();
                    let dts = time - inner.base_time.unwrap();
                    gst_debug!(SOCKET_CAT, obj: &inner.element, "Read {} bytes at {} (clock {})", len, dts, time);
                    (len, dts)
                }
            }
        };

        let mut buffer = self.1.take().unwrap().into_buffer();
        {
            let buffer = buffer.get_mut().unwrap();
            if len < buffer.get_size() {
                buffer.set_size(len);
            }
            buffer.set_dts(time);
        }

        Ok(Async::Ready(Some(buffer)))
    }
}
