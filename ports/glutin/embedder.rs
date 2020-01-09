/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Implements the global methods required by Servo (not window/gl/compositor related).

use crate::events_loop::EventsLoop;
use servo::canvas::{SurfaceProviders, WebGlExecutor};
use servo::compositing::windowing::EmbedderMethods;
use servo::embedder_traits::EventLoopWaker;
use servo::servo_config::{opts, pref};
use std::cell::RefCell;
use std::rc::Rc;

pub struct EmbedderCallbacks {
    events_loop: Rc<RefCell<EventsLoop>>,
}

impl EmbedderCallbacks {
    pub fn new(
        events_loop: Rc<RefCell<EventsLoop>>,
    ) -> EmbedderCallbacks {
        EmbedderCallbacks {
            events_loop,
        }
    }
}

impl EmbedderMethods for EmbedderCallbacks {
    fn create_event_loop_waker(&mut self) -> Box<dyn EventLoopWaker> {
        self.events_loop.borrow().create_event_loop_waker()
    }

    fn register_webxr(
        &mut self,
        xr: &mut webxr::MainThreadRegistry,
        _executor: WebGlExecutor,
        _surface_provider_registration: SurfaceProviders
    ) {
        if pref!(dom.webxr.test) {
            xr.register_mock(webxr::headless::HeadlessMockDiscovery::new());
        } else if !opts::get().headless && pref!(dom.webxr.glwindow) {
            // TODO: register the glwindow XR device
        }
    }
}
