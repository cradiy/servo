/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::reflector::reflect_dom_object_with_proto;
use stylo_atoms::Atom;

use crate::dom::bindings::codegen::Bindings::AnimationPlaybackEventBinding::{
    AnimationPlaybackEventInit, AnimationPlaybackEventMethods,
};
use crate::dom::bindings::codegen::Bindings::EventBinding::EventMethods;
use crate::dom::bindings::inheritance::Castable;
use crate::dom::bindings::num::Finite;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::event::Event;
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct AnimationPlaybackEvent {
    event: Event,
    current_time: Option<Finite<f64>>,
    timeline_time: Option<Finite<f64>>,
}

impl AnimationPlaybackEvent {
    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        type_: Atom,
        init: &AnimationPlaybackEventInit,
    ) -> DomRoot<Self> {
        Self::new_with_proto(cx, window, None, type_, init)
    }

    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        type_: Atom,
        init: &AnimationPlaybackEventInit,
    ) -> DomRoot<Self> {
        let event = reflect_dom_object_with_proto(
            cx,
            Box::new(Self {
                event: Event::new_inherited(),
                current_time: init.currentTime,
                timeline_time: init.timelineTime,
            }),
            window,
            proto,
        );
        event
            .upcast::<Event>()
            .init_event(type_, init.parent.bubbles, init.parent.cancelable);
        event.upcast::<Event>().set_composed(init.parent.composed);
        event
    }
}

impl AnimationPlaybackEventMethods<crate::DomTypeHolder> for AnimationPlaybackEvent {
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        type_: DOMString,
        init: &AnimationPlaybackEventInit,
    ) -> DomRoot<Self> {
        Self::new_with_proto(cx, window, proto, Atom::from(type_), init)
    }
    fn GetCurrentTime(&self) -> Option<Finite<f64>> {
        self.current_time
    }
    fn GetTimelineTime(&self) -> Option<Finite<f64>> {
        self.timeline_time
    }
    fn IsTrusted(&self) -> bool {
        self.upcast::<Event>().IsTrusted()
    }
}
