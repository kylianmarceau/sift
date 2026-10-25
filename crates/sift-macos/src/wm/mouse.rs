//! Opt-in pointer observation. The main thread only coalesces points; native
//! hit testing stays on a separate worker and is capped at 60 Hz while moving.
//! With no pending motion the worker sleeps indefinitely, without a timer.
use super::ffi::*;
use super::{Controller, Event, worker};
use block2::RcBlock;
use objc2::rc::Retained;
use objc2::runtime::AnyObject;
use objc2_app_kit::{NSEvent, NSEventMask, NSEventType};
use sift_wm::{MouseConfig, Rect};
use std::collections::VecDeque;
use std::ptr::NonNull;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

pub(crate) struct Drag {
    pub id: u32,
    pub before: Rect,
    pub after: Rect,
    pub point: Point,
}
enum Edge {
    Down(Point),
    Up(Point),
    Cancel,
}
#[derive(Default)]
struct Pending {
    edges: VecDeque<Edge>,
    hover: Option<Point>,
    stop: bool,
}
type Mailbox = Arc<(Mutex<Pending>, Condvar)>;

pub(super) struct Monitor {
    pub config: MouseConfig,
    token: Retained<AnyObject>,
    mailbox: Mailbox,
}
impl Monitor {
    pub fn start(controller: Controller, config: MouseConfig) -> Result<Self, String> {
        let mailbox: Mailbox = Arc::default();
        let input = mailbox.clone();
        let target = controller.clone();
        std::thread::Builder::new()
            .name("sift-mouse-ax".into())
            .spawn(move || run(input, target))
            .map_err(|e| e.to_string())?;
        let input = mailbox.clone();
        let mut mask = NSEventMask::empty();
        if config.focus_follows_mouse {
            mask |= NSEventMask::MouseMoved;
        }
        if config.drag_to_rearrange {
            mask |= NSEventMask::LeftMouseDown | NSEventMask::LeftMouseUp;
        }
        let block = RcBlock::new(move |event: NonNull<NSEvent>| {
            let event = unsafe { event.as_ref() };
            let point = NSEvent::mouseLocation();
            let snapshot = controller.snapshot();
            let height = snapshot
                .monitors
                .iter()
                .find(|m| m.main)
                .map(|m| m.frame.height)
                .unwrap_or(0.);
            let point = Point {
                x: point.x,
                y: height - point.y,
            };
            let (lock, wake) = &*input;
            let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
            if pending.stop {
                return;
            }
            match event.r#type() {
                NSEventType::MouseMoved => pending.hover = Some(point),
                NSEventType::LeftMouseDown => {
                    pending.hover = None;
                    let _ = controller.tx.try_send(Event::MouseHold(true));
                    pending.edges.push_back(Edge::Down(point));
                }
                NSEventType::LeftMouseUp => {
                    pending.hover = None;
                    pending.edges.push_back(Edge::Up(point));
                }
                _ => return,
            }
            if pending.edges.len() > 32 {
                pending.edges.clear();
                pending.edges.push_back(Edge::Cancel);
            }
            wake.notify_one();
        });
        let Some(token) = NSEvent::addGlobalMonitorForEventsMatchingMask_handler(mask, &block)
        else {
            stop(&mailbox);
            return Err("macOS did not install the optional mouse monitor".into());
        };
        Ok(Self {
            config,
            token,
            mailbox,
        })
    }
}
impl Drop for Monitor {
    fn drop(&mut self) {
        unsafe {
            NSEvent::removeMonitor(&self.token);
        }
        stop(&self.mailbox);
    }
}
fn stop(mailbox: &Mailbox) {
    let (lock, wake) = &**mailbox;
    lock.lock().unwrap_or_else(|e| e.into_inner()).stop = true;
    wake.notify_one();
}

struct CancelOnExit {
    mailbox: Mailbox,
    tx: std::sync::mpsc::SyncSender<Event>,
}
impl Drop for CancelOnExit {
    fn drop(&mut self) {
        stop(&self.mailbox);
        let _ = self.tx.send(Event::DragEnd(None));
    }
}

fn run(mailbox: Mailbox, controller: Controller) {
    let tx = &controller.tx;
    let _cancel = CancelOnExit {
        mailbox: mailbox.clone(),
        tx: tx.clone(),
    };
    let Some(system) = (unsafe { Owned::new(AXUIElementCreateSystemWide()) }) else {
        return;
    };
    unsafe {
        AXUIElementSetMessagingTimeout(system.0, 0.05);
    }
    let mut dragged: Option<(u32, Owned, Rect)> = None;
    let mut last_hover = Instant::now() - Duration::from_secs(1);
    loop {
        let (lock, wake) = &*mailbox;
        let mut pending = lock.lock().unwrap_or_else(|e| e.into_inner());
        while pending.edges.is_empty() && pending.hover.is_none() && !pending.stop {
            pending = wake.wait(pending).unwrap_or_else(|e| e.into_inner());
        }
        if pending.stop {
            break;
        }
        let edge = pending.edges.pop_front();
        if edge.is_none() && last_hover.elapsed() < Duration::from_millis(16) {
            let remaining = Duration::from_millis(16).saturating_sub(last_hover.elapsed());
            let _ = wake
                .wait_timeout(pending, remaining)
                .unwrap_or_else(|e| e.into_inner());
            continue;
        }
        let hover = if edge.is_none() {
            pending.hover.take()
        } else {
            None
        };
        drop(pending);
        match edge {
            Some(Edge::Down(point)) => {
                dragged = hit(system.0, point).and_then(|(id, window)| {
                    if !controller
                        .snapshot()
                        .windows
                        .iter()
                        .any(|w| w.id == id && w.is_tiled())
                    {
                        return None;
                    }
                    worker::frame(window.0).map(|frame| (id, window, frame))
                });
                if tx.send(Event::MouseHold(true)).is_err() {
                    break;
                }
            }
            Some(Edge::Up(point)) => {
                let result = dragged.take().and_then(|(id, window, before)| {
                    worker::frame(window.0).map(|after| Drag {
                        id,
                        before,
                        after,
                        point,
                    })
                });
                if tx.send(Event::DragEnd(result)).is_err() {
                    break;
                }
            }
            Some(Edge::Cancel) => {
                dragged = None;
                if tx.send(Event::DragEnd(None)).is_err() {
                    break;
                }
            }
            None => {
                if let Some(point) = hover {
                    last_hover = Instant::now();
                    if dragged.is_none() {
                        if let Some((id, _)) = hit(system.0, point) {
                            let snapshot = controller.snapshot();
                            if snapshot.focused != Some(id)
                                && snapshot.windows.iter().any(|w| w.id == id && w.available())
                            {
                                let _ = tx.try_send(Event::Hover(id));
                            }
                        }
                    }
                }
            }
        }
    }
}
fn hit(system: Ref, point: Point) -> Option<(u32, Owned)> {
    let mut element = std::ptr::null();
    if unsafe {
        AXUIElementCopyElementAtPosition(system, point.x as f32, point.y as f32, &mut element)
    } != 0
    {
        return None;
    }
    let element = unsafe { Owned::new(element) }?;
    let window = if worker::attribute(element.0, "AXRole")
        .is_some_and(|role| string(role.0) == "AXWindow")
    {
        element
    } else {
        worker::attribute(element.0, "AXWindow")?
    };
    unsafe {
        AXUIElementSetMessagingTimeout(window.0, 0.05);
    }
    Some((worker::window_id(window.0)?, window))
}

pub(super) fn move_cursor(rect: Rect, lazy: bool) -> Result<(), String> {
    if !rect.valid() {
        return Err("invalid cursor destination".into());
    }
    if lazy {
        if let Some(event) = unsafe { Owned::new(CGEventCreate(std::ptr::null())) } {
            let current = unsafe { CGEventGetLocation(event.0) };
            if rect.contains(current.x, current.y) {
                return Ok(());
            }
        }
    }
    let (x, y) = rect.center();
    let error = unsafe { CGWarpMouseCursorPosition(Point { x, y }) };
    if error == 0 {
        Ok(())
    } else {
        Err(format!("macOS refused cursor positioning ({error})"))
    }
}
