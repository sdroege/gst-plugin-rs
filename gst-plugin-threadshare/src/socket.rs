// Copyright (C) 2018 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2018 LEE Dongjun <redongjun@gmail.com>
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

use either::Either;
use futures::{channel::oneshot, prelude::*};

use gst;
use gst::prelude::*;
use gst::{gst_debug, gst_error};

use lazy_static::lazy_static;

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{self, Poll};

use tokio_executor::current_thread as tokio_current_thread;

use super::iocontext::*;

lazy_static! {
    static ref SOCKET_CAT: gst::DebugCategory = gst::DebugCategory::new(
        "ts-socket",
        gst::DebugColorFlags::empty(),
        Some("Thread-sharing Socket"),
    );
}

pub struct Socket<T: SocketRead + 'static>(Arc<Mutex<SocketInner<T>>>);

#[derive(PartialEq, Eq, Debug)]
enum SocketState {
    Unscheduled,
    Scheduled,
    Running,
    Shutdown,
}

pub trait SocketRead: Send + Unpin {
    const DO_TIMESTAMP: bool;

    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut task::Context<'_>,
        buf: &mut [u8],
    ) -> Poll<io::Result<(usize, Option<std::net::SocketAddr>)>>;
}

struct SocketInner<T: SocketRead + 'static> {
    element: gst::Element,
    state: SocketState,
    reader: Pin<Box<T>>,
    buffer_pool: gst::BufferPool,
    waker: Option<task::Waker>,
    shutdown_receiver: Option<oneshot::Receiver<()>>,
    clock: Option<gst::Clock>,
    base_time: Option<gst::ClockTime>,
}

impl<T: SocketRead + 'static> Socket<T> {
    pub fn new(element: &gst::Element, reader: T, buffer_pool: gst::BufferPool) -> Self {
        Socket(Arc::new(Mutex::new(SocketInner::<T> {
            element: element.clone(),
            state: SocketState::Unscheduled,
            reader: Pin::new(Box::new(reader)),
            buffer_pool,
            waker: None,
            shutdown_receiver: None,
            clock: None,
            base_time: None,
        })))
    }

    pub fn schedule<F, G, Fut>(
        &self,
        io_context: &IOContext,
        func: F,
        err_func: G,
    ) -> Result<(), ()>
    where
        F: Fn((gst::Buffer, Option<std::net::SocketAddr>)) -> Fut + Send + 'static,
        Fut: Future<Output = Result<(), gst::FlowError>> + Send + 'static,
        G: FnOnce(Either<gst::FlowError, io::Error>) + Send + 'static,
    {
        // Ready->Paused
        //
        // Need to wait for a possible shutdown to finish first
        // spawn() on the reactor, change state to Scheduled
        let stream = SocketStream::<T>(self.clone(), None);

        let mut inner = self.0.lock().unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Scheduling socket");
        if inner.state == SocketState::Scheduled {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket already scheduled");
            return Ok(());
        }

        assert_eq!(inner.state, SocketState::Unscheduled);
        inner.state = SocketState::Scheduled;
        if inner.buffer_pool.set_active(true).is_err() {
            gst_error!(SOCKET_CAT, obj: &inner.element, "Failed to activate buffer pool");
            return Err(());
        }

        let (sender, receiver) = oneshot::channel();
        inner.shutdown_receiver = Some(receiver);

        let element_clone = inner.element.clone();
        io_context.spawn(
            stream
                .try_for_each(move |(buffer, saddr)| {
                    func((buffer, saddr)).into_future().map_err(Either::Left)
                })
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

                    future::ready(())
                }),
        );
        Ok(())
    }

    pub fn unpause(&self, clock: Option<gst::Clock>, base_time: Option<gst::ClockTime>) {
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
        inner.clock = clock;
        inner.base_time = base_time;

        if let Some(waker) = inner.waker.take() {
            waker.wake();
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

        if let Some(waker) = inner.waker.take() {
            waker.wake();
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

        if let Some(waker) = inner.waker.take() {
            waker.wake();
        }

        let shutdown_receiver = inner.shutdown_receiver.take().unwrap();
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Waiting for socket to shut down");
        drop(inner);

        tokio_current_thread::block_on_all(shutdown_receiver).expect("Already shut down");

        let mut inner = self.0.lock().unwrap();
        inner.state = SocketState::Unscheduled;
        let _ = inner.buffer_pool.set_active(false);
        gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket shut down");
    }
}

impl<T: SocketRead + Unpin + 'static> Clone for Socket<T> {
    fn clone(&self) -> Self {
        Socket::<T>(self.0.clone())
    }
}

impl<T: SocketRead + 'static> Drop for SocketInner<T> {
    fn drop(&mut self) {
        assert_eq!(self.state, SocketState::Unscheduled);
    }
}

struct SocketStream<T: SocketRead + 'static>(
    Socket<T>,
    Option<gst::MappedBuffer<gst::buffer::Writable>>,
);

impl<T: SocketRead + 'static> Stream for SocketStream<T> {
    type Item =
        Result<(gst::Buffer, Option<std::net::SocketAddr>), Either<gst::FlowError, io::Error>>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut task::Context<'_>) -> Poll<Option<Self::Item>> {
        // take the mapped_buffer before locking the socket so as to please the mighty borrow checker
        let mut mapped_buffer = self.1.take();

        let mut inner = (self.0).0.lock().unwrap();
        if inner.state == SocketState::Shutdown {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket shutting down");
            return Poll::Ready(None);
        } else if inner.state == SocketState::Scheduled {
            gst_debug!(SOCKET_CAT, obj: &inner.element, "Socket not running");
            inner.waker = Some(cx.waker().clone());
            drop(inner);
            self.1 = mapped_buffer;
            return Poll::Pending;
        }

        assert_eq!(inner.state, SocketState::Running);

        gst_debug!(SOCKET_CAT, obj: &inner.element, "Trying to read data");
        let (len, saddr, time) = {
            let buffer = match mapped_buffer {
                Some(ref mut buffer) => buffer,
                None => match inner.buffer_pool.acquire_buffer(None) {
                    Ok(buffer) => {
                        mapped_buffer = Some(buffer.into_mapped_buffer_writable().unwrap());
                        mapped_buffer.as_mut().unwrap()
                    }
                    Err(err) => {
                        gst_debug!(SOCKET_CAT, obj: &inner.element, "Failed to acquire buffer {:?}", err);
                        return Poll::Ready(Some(Err(Either::Left(err))));
                    }
                },
            };

            match inner.reader.as_mut().poll_read(cx, buffer.as_mut_slice()) {
                Poll::Pending => {
                    gst_debug!(SOCKET_CAT, obj: &inner.element, "No data available");
                    inner.waker = Some(cx.waker().clone());
                    drop(inner);
                    self.1 = mapped_buffer;
                    return Poll::Pending;
                }
                Poll::Ready(Err(err)) => {
                    gst_debug!(SOCKET_CAT, obj: &inner.element, "Read error {:?}", err);
                    return Poll::Ready(Some(Err(Either::Right(err))));
                }
                Poll::Ready(Ok((len, saddr))) => {
                    let dts = if T::DO_TIMESTAMP {
                        let time = inner.clock.as_ref().unwrap().get_time();
                        let running_time = time - inner.base_time.unwrap();
                        gst_debug!(SOCKET_CAT, obj: &inner.element, "Read {} bytes at {} (clock {})", len, running_time, time);
                        running_time
                    } else {
                        gst_debug!(SOCKET_CAT, obj: &inner.element, "Read {} bytes", len);
                        gst::CLOCK_TIME_NONE
                    };
                    (len, saddr, dts)
                }
            }
        };

        let mut buffer = mapped_buffer.unwrap().into_buffer();
        {
            let buffer = buffer.get_mut().unwrap();
            if len < buffer.get_size() {
                buffer.set_size(len);
            }
            buffer.set_dts(time);
        }

        Poll::Ready(Some(Ok((buffer, saddr))))
    }
}
