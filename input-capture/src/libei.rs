use ashpd::{
    desktop::{
        input_capture::{Activated, Barrier, BarrierID, Capabilities, InputCapture, Region, Zones},
        ResponseError, Session,
    },
    enumflags2::BitFlags,
};
use futures::StreamExt;
use reis::{
    ei::{self, keyboard::KeyState},
    eis::button::ButtonState,
    event::{DeviceCapability, EiEvent},
    tokio::{EiConvertEventStream, EiEventStream},
};
use std::{
    cell::Cell,
    collections::HashMap,
    io,
    os::unix::net::UnixStream,
    pin::Pin,
    rc::Rc,
    task::{ready, Context, Poll},
};
use tokio::{
    sync::mpsc::{Receiver, Sender},
    task::JoinHandle,
};

use futures_core::Stream;
use once_cell::sync::Lazy;

use input_event::{Event, KeyboardEvent, PointerEvent};

use crate::error::{CaptureError, ReisConvertEventStreamError};

use super::{
    error::LibeiCaptureCreationError, CaptureHandle, InputCapture as LanMouseInputCapture, Position,
};

#[derive(Debug)]
enum ProducerEvent {
    Release,
    Create(CaptureHandle, Position),
    Destroy(CaptureHandle),
}

#[allow(dead_code)]
pub struct LibeiInputCapture<'a> {
    input_capture: Pin<Box<InputCapture<'a>>>,
    libei_task: JoinHandle<Result<(), CaptureError>>,
    event_rx: tokio::sync::mpsc::Receiver<(CaptureHandle, Event)>,
    notify_tx: tokio::sync::mpsc::Sender<ProducerEvent>,
}

static INTERFACES: Lazy<HashMap<&'static str, u32>> = Lazy::new(|| {
    let mut m = HashMap::new();
    m.insert("ei_connection", 1);
    m.insert("ei_callback", 1);
    m.insert("ei_pingpong", 1);
    m.insert("ei_seat", 1);
    m.insert("ei_device", 2);
    m.insert("ei_pointer", 1);
    m.insert("ei_pointer_absolute", 1);
    m.insert("ei_scroll", 1);
    m.insert("ei_button", 1);
    m.insert("ei_keyboard", 1);
    m.insert("ei_touchscreen", 1);
    m
});

fn pos_to_barrier(r: &Region, pos: Position) -> (i32, i32, i32, i32) {
    let (x, y) = (r.x_offset(), r.y_offset());
    let (width, height) = (r.width() as i32, r.height() as i32);
    match pos {
        Position::Left => (x, y, x, y + height - 1), // start pos, end pos, inclusive
        Position::Right => (x + width, y, x + width, y + height - 1),
        Position::Top => (x, y, x + width - 1, y),
        Position::Bottom => (x, y + height, x + width - 1, y + height),
    }
}

fn select_barriers(
    zones: &Zones,
    clients: &Vec<(CaptureHandle, Position)>,
    next_barrier_id: &mut u32,
) -> (Vec<Barrier>, HashMap<BarrierID, CaptureHandle>) {
    let mut client_for_barrier = HashMap::new();
    let mut barriers: Vec<Barrier> = vec![];

    for (handle, pos) in clients {
        let mut client_barriers = zones
            .regions()
            .iter()
            .map(|r| {
                let id = *next_barrier_id;
                *next_barrier_id = id + 1;
                let position = pos_to_barrier(r, *pos);
                client_for_barrier.insert(id, *handle);
                Barrier::new(id, position)
            })
            .collect();
        barriers.append(&mut client_barriers);
    }
    (barriers, client_for_barrier)
}

async fn update_barriers(
    input_capture: &InputCapture<'_>,
    session: &Session<'_>,
    active_clients: &Vec<(CaptureHandle, Position)>,
    next_barrier_id: &mut u32,
) -> Result<HashMap<BarrierID, CaptureHandle>, ashpd::Error> {
    let zones = input_capture.zones(session).await?.response()?;
    log::debug!("zones: {zones:?}");

    let (barriers, id_map) = select_barriers(&zones, active_clients, next_barrier_id);
    log::debug!("barriers: {barriers:?}");
    log::debug!("client for barrier id: {id_map:?}");

    let response = input_capture
        .set_pointer_barriers(session, &barriers, zones.zone_set())
        .await?;
    let response = response.response()?;
    log::debug!("{response:?}");
    Ok(id_map)
}

impl<'a> Drop for LibeiInputCapture<'a> {
    fn drop(&mut self) {
        self.libei_task.abort();
    }
}

async fn create_session<'a>(
    input_capture: &'a InputCapture<'a>,
) -> std::result::Result<(Session<'a>, BitFlags<Capabilities>), ashpd::Error> {
    log::debug!("creating input capture session");
    let (session, capabilities) = loop {
        match input_capture
            .create_session(
                &ashpd::WindowIdentifier::default(),
                Capabilities::Keyboard | Capabilities::Pointer | Capabilities::Touchscreen,
            )
            .await
        {
            Ok(s) => break s,
            Err(ashpd::Error::Response(ResponseError::Cancelled)) => continue,
            o => o?,
        };
    };
    log::debug!("capabilities: {capabilities:?}");
    Ok((session, capabilities))
}

async fn connect_to_eis(
    input_capture: &InputCapture<'_>,
    session: &Session<'_>,
) -> Result<(ei::Context, EiConvertEventStream), CaptureError> {
    log::debug!("connect_to_eis");
    let fd = input_capture.connect_to_eis(session).await?;

    // create unix stream from fd
    let stream = UnixStream::from(fd);
    stream.set_nonblocking(true)?;

    // create ei context
    let context = ei::Context::new(stream)?;
    let mut event_stream = EiEventStream::new(context.clone())?;
    let response = reis::tokio::ei_handshake(
        &mut event_stream,
        "de.feschber.LanMouse",
        ei::handshake::ContextType::Receiver,
        &INTERFACES,
    )
    .await?;
    let event_stream = EiConvertEventStream::new(event_stream, response.serial);

    Ok((context, event_stream))
}

async fn libei_event_handler(
    mut ei_event_stream: EiConvertEventStream,
    context: ei::Context,
    event_tx: Sender<(CaptureHandle, Event)>,
    current_client: Rc<Cell<Option<CaptureHandle>>>,
) -> Result<(), CaptureError> {
    loop {
        let ei_event = ei_event_stream
            .next()
            .await
            .ok_or(CaptureError::EndOfStream)?
            .map_err(ReisConvertEventStreamError::from)?;
        log::trace!("from ei: {ei_event:?}");
        let client = current_client.get();
        handle_ei_event(ei_event, client, &context, &event_tx).await;
    }
}

async fn wait_for_active_client(
    notify_rx: &mut Receiver<ProducerEvent>,
    active_clients: &mut Vec<(CaptureHandle, Position)>,
) {
    // wait for a client update
    while let Some(producer_event) = notify_rx.recv().await {
        if let ProducerEvent::Create(c, p) = producer_event {
            handle_producer_event(ProducerEvent::Create(c, p), active_clients);
            break;
        }
    }
}

impl<'a> LibeiInputCapture<'a> {
    pub async fn new() -> std::result::Result<Self, LibeiCaptureCreationError> {
        let input_capture = Box::pin(InputCapture::new().await?);
        let input_capture_ptr = input_capture.as_ref().get_ref() as *const InputCapture<'static>;
        let first_session = Some(create_session(unsafe { &*input_capture_ptr }).await?);

        let (event_tx, event_rx) = tokio::sync::mpsc::channel(32);
        let (notify_tx, notify_rx) = tokio::sync::mpsc::channel(32);
        let capture = do_capture(input_capture_ptr, notify_rx, first_session, event_tx);
        let libei_task = tokio::task::spawn_local(capture);

        let producer = Self {
            input_capture,
            event_rx,
            libei_task,
            notify_tx,
        };

        Ok(producer)
    }
}

async fn do_capture<'a>(
    input_capture_ptr: *const InputCapture<'static>,
    mut notify_rx: Receiver<ProducerEvent>,
    mut first_session: Option<(Session<'a>, BitFlags<Capabilities>)>,
    event_tx: Sender<(CaptureHandle, Event)>,
) -> Result<(), CaptureError> {
    /* safety: libei_task does not outlive Self */
    let input_capture = unsafe { &*input_capture_ptr };

    let mut active_clients: Vec<(CaptureHandle, Position)> = vec![];
    let mut next_barrier_id = 1u32;

    /* there is a bug in xdg-remote-desktop-portal-gnome / mutter that
     * prevents receiving further events after a session has been disabled once.
     * Therefore the session needs to recreated when the barriers are updated */

    loop {
        // otherwise it asks to capture input even with no active clients
        if active_clients.is_empty() {
            wait_for_active_client(&mut notify_rx, &mut active_clients).await;
            if notify_rx.is_closed() {
                break Ok(());
            } else {
                continue;
            }
        }

        let current_client = Rc::new(Cell::new(None));

        // create session
        let (session, _) = match first_session.take() {
            Some(s) => s,
            _ => create_session(input_capture).await?,
        };

        // connect to eis server
        let (context, ei_event_stream) = connect_to_eis(input_capture, &session).await?;

        // async event task
        let mut ei_task: JoinHandle<Result<(), CaptureError>> =
            tokio::task::spawn_local(libei_event_handler(
                ei_event_stream,
                context,
                event_tx.clone(),
                current_client.clone(),
            ));

        let mut activated = input_capture.receive_activated().await?;
        let mut zones_changed = input_capture.receive_zones_changed().await?;

        // set barriers
        let client_for_barrier_id = update_barriers(
            input_capture,
            &session,
            &active_clients,
            &mut next_barrier_id,
        )
        .await?;

        log::debug!("enabling session");
        input_capture.enable(&session).await?;

        loop {
            tokio::select! {
                activated = activated.next() => {
                    let activated = activated.ok_or(CaptureError::ActivationClosed)?;
                    log::debug!("activated: {activated:?}");

                    let client = *client_for_barrier_id
                        .get(&activated.barrier_id())
                        .expect("invalid barrier id");
                    current_client.replace(Some(client));

                    if event_tx.send((client, Event::Enter())).await.is_err() {
                        break;
                    };

                    tokio::select! {
                        producer_event = notify_rx.recv() => {
                            let producer_event = producer_event.expect("channel closed");
                            if handle_producer_event(producer_event, &mut active_clients) {
                                break; /* clients updated */
                            }
                        }
                        zones_changed = zones_changed.next() => {
                            log::debug!("zones changed: {zones_changed:?}");
                            break;
                        }
                        res = &mut ei_task => {
                            if let Err(e) = res.expect("ei task paniced") {
                                log::warn!("libei task exited: {e}");
                            }
                            break;
                        }
                    }
                    release_capture(
                        input_capture,
                        &session,
                        activated,
                        client,
                        &active_clients,
                    ).await?;
                }
                producer_event = notify_rx.recv() => {
                    let producer_event = producer_event.expect("channel closed");
                    if handle_producer_event(producer_event, &mut active_clients) {
                        /* clients updated */
                        break;
                    }
                },
                res = &mut ei_task => {
                    if let Err(e) = res.expect("ei task paniced") {
                        log::warn!("libei task exited: {e}");
                    }
                    break;
                }
            }
        }
        ei_task.abort();
        input_capture.disable(&session).await?;
        if event_tx.is_closed() {
            break Ok(());
        }
    }
}

async fn release_capture(
    input_capture: &InputCapture<'_>,
    session: &Session<'_>,
    activated: Activated,
    current_client: CaptureHandle,
    active_clients: &[(CaptureHandle, Position)],
) -> Result<(), CaptureError> {
    log::debug!("releasing input capture {}", activated.activation_id());
    let (x, y) = activated.cursor_position();
    let pos = active_clients
        .iter()
        .filter(|(c, _)| *c == current_client)
        .map(|(_, p)| p)
        .next()
        .unwrap(); // FIXME
    let (dx, dy) = match pos {
        // offset cursor position to not enter again immediately
        Position::Left => (1., 0.),
        Position::Right => (-1., 0.),
        Position::Top => (0., 1.),
        Position::Bottom => (0., -1.),
    };
    // release 1px to the right of the entered zone
    let cursor_position = (x as f64 + dx, y as f64 + dy);
    input_capture
        .release(session, activated.activation_id(), cursor_position)
        .await?;
    Ok(())
}

fn handle_producer_event(
    producer_event: ProducerEvent,
    active_clients: &mut Vec<(CaptureHandle, Position)>,
) -> bool {
    log::debug!("handling event: {producer_event:?}");
    match producer_event {
        ProducerEvent::Release => false,
        ProducerEvent::Create(c, p) => {
            active_clients.push((c, p));
            true
        }
        ProducerEvent::Destroy(c) => {
            active_clients.retain(|(h, _)| *h != c);
            true
        }
    }
}

async fn handle_ei_event(
    ei_event: EiEvent,
    current_client: Option<CaptureHandle>,
    context: &ei::Context,
    event_tx: &Sender<(CaptureHandle, Event)>,
) {
    match ei_event {
        EiEvent::SeatAdded(s) => {
            s.seat.bind_capabilities(&[
                DeviceCapability::Pointer,
                DeviceCapability::PointerAbsolute,
                DeviceCapability::Keyboard,
                DeviceCapability::Touch,
                DeviceCapability::Scroll,
                DeviceCapability::Button,
            ]);
            context.flush().unwrap();
        }
        EiEvent::SeatRemoved(_) => {}
        EiEvent::DeviceAdded(_) => {}
        EiEvent::DeviceRemoved(_) => {}
        EiEvent::DevicePaused(_) => {}
        EiEvent::DeviceResumed(_) => {}
        EiEvent::KeyboardModifiers(mods) => {
            let modifier_event = KeyboardEvent::Modifiers {
                mods_depressed: mods.depressed,
                mods_latched: mods.latched,
                mods_locked: mods.locked,
                group: mods.group,
            };
            if let Some(current_client) = current_client {
                event_tx
                    .send((current_client, Event::Keyboard(modifier_event)))
                    .await
                    .unwrap();
            }
        }
        EiEvent::Frame(_) => {}
        EiEvent::DeviceStartEmulating(_) => {
            log::debug!("START EMULATING =============>");
        }
        EiEvent::DeviceStopEmulating(_) => {
            log::debug!("==================> STOP EMULATING");
        }
        EiEvent::PointerMotion(motion) => {
            let motion_event = PointerEvent::Motion {
                time: motion.time as u32,
                dx: motion.dx as f64,
                dy: motion.dy as f64,
            };
            if let Some(current_client) = current_client {
                event_tx
                    .send((current_client, Event::Pointer(motion_event)))
                    .await
                    .unwrap();
            }
        }
        EiEvent::PointerMotionAbsolute(_) => {}
        EiEvent::Button(button) => {
            let button_event = PointerEvent::Button {
                time: button.time as u32,
                button: button.button,
                state: match button.state {
                    ButtonState::Released => 0,
                    ButtonState::Press => 1,
                },
            };
            if let Some(current_client) = current_client {
                event_tx
                    .send((current_client, Event::Pointer(button_event)))
                    .await
                    .unwrap();
            }
        }
        EiEvent::ScrollDelta(delta) => {
            if let Some(handle) = current_client {
                let mut events = vec![];
                if delta.dy != 0. {
                    events.push(PointerEvent::Axis {
                        time: 0,
                        axis: 0,
                        value: delta.dy as f64,
                    });
                }
                if delta.dx != 0. {
                    events.push(PointerEvent::Axis {
                        time: 0,
                        axis: 1,
                        value: delta.dx as f64,
                    });
                }
                for event in events {
                    event_tx
                        .send((handle, Event::Pointer(event)))
                        .await
                        .unwrap();
                }
            }
        }
        EiEvent::ScrollStop(_) => {}
        EiEvent::ScrollCancel(_) => {}
        EiEvent::ScrollDiscrete(scroll) => {
            if scroll.discrete_dy != 0 {
                let event = PointerEvent::AxisDiscrete120 {
                    axis: 0,
                    value: scroll.discrete_dy,
                };
                if let Some(current_client) = current_client {
                    event_tx
                        .send((current_client, Event::Pointer(event)))
                        .await
                        .unwrap();
                }
            }
            if scroll.discrete_dx != 0 {
                let event = PointerEvent::AxisDiscrete120 {
                    axis: 1,
                    value: scroll.discrete_dx,
                };
                if let Some(current_client) = current_client {
                    event_tx
                        .send((current_client, Event::Pointer(event)))
                        .await
                        .unwrap();
                }
            };
        }
        EiEvent::KeyboardKey(key) => {
            let key_event = KeyboardEvent::Key {
                key: key.key,
                state: match key.state {
                    KeyState::Press => 1,
                    KeyState::Released => 0,
                },
                time: key.time as u32,
            };
            if let Some(current_client) = current_client {
                event_tx
                    .send((current_client, Event::Keyboard(key_event)))
                    .await
                    .unwrap();
            }
        }
        EiEvent::TouchDown(_) => {}
        EiEvent::TouchUp(_) => {}
        EiEvent::TouchMotion(_) => {}
        EiEvent::Disconnected(d) => {
            log::error!("disconnect: {d:?}");
        }
    }
}

impl<'a> LanMouseInputCapture for LibeiInputCapture<'a> {
    fn create(&mut self, handle: super::CaptureHandle, pos: super::Position) -> io::Result<()> {
        let notify_tx = self.notify_tx.clone();
        tokio::task::spawn_local(async move {
            let _ = notify_tx.send(ProducerEvent::Create(handle, pos)).await;
        });
        Ok(())
    }

    fn destroy(&mut self, handle: super::CaptureHandle) -> io::Result<()> {
        let notify_tx = self.notify_tx.clone();
        tokio::task::spawn_local(async move {
            let _ = notify_tx.send(ProducerEvent::Destroy(handle)).await;
        });
        Ok(())
    }

    fn release(&mut self) -> io::Result<()> {
        let notify_tx = self.notify_tx.clone();
        tokio::task::spawn_local(async move {
            let _ = notify_tx.send(ProducerEvent::Release).await;
        });
        Ok(())
    }
}

impl<'a> Stream for LibeiInputCapture<'a> {
    type Item = io::Result<(CaptureHandle, Event)>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match ready!(self.event_rx.poll_recv(cx)) {
            None => Poll::Ready(None),
            Some(e) => Poll::Ready(Some(Ok(e))),
        }
    }
}
