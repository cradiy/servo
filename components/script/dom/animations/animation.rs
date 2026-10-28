/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::HandleObject;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::DocumentBinding::DocumentMethods;
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::error::{Error, Fallible};
use script_bindings::inheritance::Castable;
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_weak_referenceable_dom_object_with_proto;
use script_bindings::root::{Dom, DomRoot};
use script_bindings::str::DOMString;

use crate::dom::animationeffect::AnimationEffect;
use crate::dom::animationplaybackevent::AnimationPlaybackEvent;
use crate::dom::bindings::codegen::Bindings::AnimationBinding::{
    AnimationMethods, AnimationPlayState,
};
use crate::dom::bindings::codegen::Bindings::AnimationPlaybackEventBinding::AnimationPlaybackEventInit;
use crate::dom::bindings::codegen::Bindings::EventBinding::EventInit;
use crate::dom::bindings::refcounted::Trusted;
use crate::dom::bindings::reflector::DomGlobal;
use crate::dom::bindings::root::MutNullableDom;
use crate::dom::event::Event;
use crate::dom::eventtarget::EventTarget;
use crate::dom::node::NodeTraits;
use crate::dom::promise::{Promise, RootedPromise, TracedPromise};
use crate::dom::types::{AnimationTimeline, DocumentTimeline, Element, KeyframeEffect, Window};
use crate::realms::enter_auto_realm;
use crate::runtime::job_queue::MicrotaskRunnable;
use stylo_atoms::Atom;

/// <https://drafts.csswg.org/web-animations-1/#animation>
#[dom_struct]
pub(crate) struct Animation {
    event_target: EventTarget,
    id: DomRefCell<DOMString>,
    window: Dom<Window>,
    timeline: MutNullableDom<AnimationTimeline>,
    associated_effect: MutNullableDom<AnimationEffect>,
    sampled_target: MutNullableDom<Element>,
    composite_order: u64,
    start_time: Cell<Option<f64>>,
    hold_time: Cell<Option<f64>>,
    playback_rate: Cell<f64>,
    pending_playback_rate: Cell<Option<f64>>,
    paused: Cell<bool>,
    pending: Cell<bool>,
    ready_promise: DomRefCell<Option<TracedPromise>>,
    finished_promise: DomRefCell<Option<TracedPromise>>,
    finished_resolved: Cell<bool>,
    finish_notification_queued: Cell<bool>,
}

#[derive(JSTraceable, MallocSizeOf)]
#[cfg_attr(crown, crown::unrooted_must_root_lint::must_root)]
struct AnimationFinishMicrotask {
    animation: Dom<Animation>,
}

impl MicrotaskRunnable for AnimationFinishMicrotask {
    fn handler(&self, cx: &mut JSContext) {
        let mut realm = enter_auto_realm(cx, &*self.animation);
        self.animation.finish_notification_queued.set(false);
        self.animation.notify_finished(&mut realm);
    }
}

impl Animation {
    fn queue_playback_event(&self, cx: &mut JSContext, type_: Atom, current_time: Option<f64>) {
        let timeline_time = self.timeline_time();
        let event = AnimationPlaybackEvent::new(
            cx,
            &self.window,
            type_.clone(),
            &AnimationPlaybackEventInit {
                parent: EventInit {
                    bubbles: false,
                    cancelable: false,
                    composed: false,
                },
                currentTime: current_time.and_then(Finite::new),
                timelineTime: timeline_time.and_then(Finite::new),
            },
        );
        if let Some(timeline) = self.timeline.get().and_then(|timeline| {
            timeline
                .downcast::<DocumentTimeline>()
                .map(DomRoot::from_ref)
        }) {
            let scheduled_time = if type_ == Atom::from("finish") {
                self.start_time
                    .get()
                    .filter(|_| self.playback_rate.get() != 0.0)
                    .map(|start| start + self.end_time() / self.playback_rate.get())
            } else {
                timeline_time
            };
            let scheduled_time = scheduled_time
                .map(|time| timeline.to_origin_relative_time(time))
                .filter(|time| time.is_finite());
            let global = timeline.global();
            let window = global.as_window();
            window.Document().animation_manager().queue_playback_event(
                window,
                self,
                &event,
                scheduled_time,
                self.composite_order,
            );
        } else {
            let target = Trusted::new(self);
            let event = Trusted::new(&*event);
            self.global()
                .task_manager()
                .dom_manipulation_task_source()
                .queue(task!(dispatch_animation_playback_event: move |cx| {
                    let target = target.root();
                    let event = event.root();
                    event.upcast::<Event>().fire(cx, target.upcast());
                }));
        }
    }

    fn effective_playback_rate(&self) -> f64 {
        self.pending_playback_rate
            .get()
            .unwrap_or(self.playback_rate.get())
    }

    fn apply_pending_playback_rate(&self) {
        if let Some(rate) = self.pending_playback_rate.take() {
            self.playback_rate.set(rate);
        }
    }

    fn play(&self, cx: &mut JSContext) -> Fallible<()> {
        if !self.paused.get()
            && self.PlayState() == AnimationPlayState::Running
            && self.pending_playback_rate.get().is_none()
        {
            return Ok(());
        }
        let rate = self.effective_playback_rate();
        let end = self.end_time();
        let current = self.current_time();
        let rewind = current.is_none()
            || (current.is_some_and(|time| {
                if rate < 0.0 {
                    time <= 0.0 || time > end
                } else if rate > 0.0 {
                    time < 0.0 || time >= end
                } else {
                    false
                }
            }));
        let time = if rewind {
            if rate < 0.0 { end } else { 0.0 }
        } else {
            current.unwrap()
        };
        if !time.is_finite() {
            return Err(Error::InvalidState(None));
        }
        if rewind || self.paused.get() || self.start_time.get().is_none() {
            self.hold_time.set(Some(time));
            self.start_time.set(None);
        }
        self.begin_pending();
        self.paused.set(false);
        self.effect_changed(cx);
        Ok(())
    }

    fn begin_pending(&self) {
        if !self.pending.replace(true) {
            *self.ready_promise.borrow_mut() = None;
        }
    }

    fn resolve_ready(&self, cx: &mut JSContext) {
        if self.pending.replace(false) {
            let promise = self
                .ready_promise
                .borrow()
                .as_ref()
                .map(|promise| promise.root(cx));
            if let Some(promise) = promise {
                promise.resolve_native(cx, self);
            }
        }
    }

    fn notify_finished(&self, cx: &mut JSContext) {
        if self.pending.get() || self.PlayState() != AnimationPlayState::Finished {
            return;
        }
        if !self.finished_resolved.replace(true) {
            let promise = self
                .finished_promise
                .borrow()
                .as_ref()
                .map(|promise| promise.root(cx));
            if let Some(promise) = promise {
                promise.resolve_native(cx, self);
            }
            self.queue_playback_event(cx, Atom::from("finish"), self.current_time());
        }
    }

    fn update_finished_promise(&self, cx: &mut JSContext) {
        if self.PlayState() != AnimationPlayState::Finished {
            if self.finished_resolved.replace(false) {
                *self.finished_promise.borrow_mut() = None;
            }
        } else if !self.pending.get()
            && !self.finished_resolved.get()
            && !self.finish_notification_queued.replace(true)
        {
            self.global().enqueue_microtask(
                cx,
                Box::new(AnimationFinishMicrotask {
                    animation: Dom::from_ref(self),
                }),
            );
        }
    }

    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        static NEXT_ORDER: AtomicU64 = AtomicU64::new(1);
        reflect_weak_referenceable_dom_object_with_proto(
            cx,
            Rc::new(Self {
                event_target: EventTarget::new_inherited(),
                id: Default::default(),
                window: Dom::from_ref(window),
                timeline: Default::default(),
                associated_effect: Default::default(),
                sampled_target: Default::default(),
                composite_order: NEXT_ORDER.fetch_add(1, Ordering::Relaxed),
                start_time: Cell::new(None),
                hold_time: Cell::new(None),
                playback_rate: Cell::new(1.0),
                pending_playback_rate: Cell::new(None),
                paused: Cell::new(false),
                pending: Cell::new(false),
                ready_promise: Default::default(),
                finished_promise: Default::default(),
                finished_resolved: Cell::new(false),
                finish_notification_queued: Cell::new(false),
            }),
            window,
            proto,
        )
    }

    fn timeline_time(&self) -> Option<f64> {
        self.timeline
            .get()
            .map(|timeline| timeline.current_time_in_seconds() * 1000.0)
    }

    fn end_time(&self) -> f64 {
        self.associated_effect
            .get()
            .map_or(0.0, |effect| effect.end_time())
    }

    pub(crate) fn current_time(&self) -> Option<f64> {
        if let Some(hold) = self.hold_time.get() {
            return Some(hold);
        }
        let time = (self.timeline_time()? - self.start_time.get()?) * self.playback_rate.get();
        Some(if self.playback_rate.get() > 0.0 {
            time.min(self.end_time())
        } else if self.playback_rate.get() < 0.0 {
            time.max(0.0)
        } else {
            time
        })
    }

    pub(crate) fn needs_ticks(&self) -> bool {
        self.timeline.get().is_some()
            && (self.pending.get()
                || (self.PlayState() == AnimationPlayState::Running
                    && self.playback_rate.get() != 0.0))
    }

    pub(crate) fn is_relevant(&self) -> bool {
        self.associated_effect
            .get()
            .and_then(|effect| {
                effect
                    .downcast::<KeyframeEffect>()
                    .and_then(|effect| effect.target())
            })
            .is_some_and(|target| target.upcast::<crate::dom::node::Node>().is_connected())
            && (self.needs_ticks() || self.sampled_target.get().is_some())
    }

    fn remove_sample(&self, cx: &mut JSContext) {
        if let Some(target) = self.sampled_target.get() {
            target
                .owner_document()
                .animation_manager()
                .remove_script_sample(cx, &target, self.composite_order);
            self.sampled_target.set(None);
        }
    }

    pub(crate) fn sample(&self, cx: &mut JSContext) {
        let Some(effect) = self.associated_effect.get() else {
            self.remove_sample(cx);
            return;
        };
        let Some(keyframes) = effect.downcast::<KeyframeEffect>() else {
            self.remove_sample(cx);
            return;
        };
        let Some(target) = keyframes.target() else {
            self.remove_sample(cx);
            return;
        };
        if !target.upcast::<crate::dom::node::Node>().is_connected() {
            self.remove_sample(cx);
            return;
        }
        let Some(progress) = effect.progress_at(self.current_time()).0 else {
            self.remove_sample(cx);
            return;
        };
        if self
            .sampled_target
            .get()
            .as_deref()
            .is_some_and(|old| old != &*target)
        {
            self.remove_sample(cx);
        }
        let values = keyframes.sample_values(&target, progress);
        target
            .owner_document()
            .animation_manager()
            .set_script_sample(
                cx,
                &target,
                self.composite_order,
                values,
                self.needs_ticks(),
            );
        self.sampled_target.set(Some(&target));
    }

    pub(crate) fn tick(&self, cx: &mut JSContext) {
        if let Some(timeline) = self.timeline.get() {
            if let Some(timeline) = timeline.downcast::<DocumentTimeline>() {
                if !servo_config::pref!(layout_animations_test_enabled)
                    && !std::ptr::eq(timeline, &*self.window.Document().Timeline())
                {
                    timeline.update(&self.window);
                }
            }
        }
        if self.pending.get() && self.timeline_time().is_some() {
            let time = self.current_time().unwrap_or(0.0);
            self.apply_pending_playback_rate();
            if self.paused.get() {
                self.hold_time.set(Some(time));
                self.start_time.set(None);
            } else if let Some(now) = self.timeline_time() {
                let rate = self.playback_rate.get();
                self.start_time
                    .set(Some(if rate == 0.0 { now } else { now - time / rate }));
                if rate == 0.0
                    || (rate > 0.0 && time >= self.end_time())
                    || (rate < 0.0 && time <= 0.0)
                {
                    self.hold_time.set(Some(time));
                } else {
                    self.hold_time.set(None);
                }
            }
            self.resolve_ready(cx);
        }
        self.sample(cx);
        self.update_finished_promise(cx);
    }

    pub(crate) fn effect_changed(&self, cx: &mut JSContext) {
        if !self.pending.get()
            && !self.paused.get()
            && self.PlayState() == AnimationPlayState::Running
        {
            if let Some(time) = self.hold_time.get() {
                self.set_time(time);
            }
        }
        self.sample(cx);
        self.update_finished_promise(cx);
        self.window
            .Document()
            .animation_manager()
            .track_script_animation(self, &self.window);
    }

    pub(crate) fn timing_changed(&self, cx: &mut JSContext, previous_time: Option<f64>) {
        self.hold_time.set(previous_time);
        self.effect_changed(cx);
    }

    fn set_time(&self, time: f64) {
        if self.pending.get() && !self.paused.get() {
            self.hold_time.set(Some(time));
            self.start_time.set(None);
            return;
        }
        let rate = self.playback_rate.get();
        if (rate > 0.0 && time >= self.end_time()) || (rate < 0.0 && time <= 0.0) {
            self.hold_time.set(Some(time));
            self.start_time.set(None);
            return;
        }
        match (
            self.timeline_time(),
            self.paused.get(),
            self.playback_rate.get(),
        ) {
            (Some(now), false, rate) if rate != 0.0 => {
                self.start_time.set(Some(now - time / rate));
                self.hold_time.set(None);
            },
            _ => {
                self.start_time.set(None);
                self.hold_time.set(Some(time));
            },
        }
    }

    fn set_effect(&self, cx: &mut JSContext, effect: Option<&AnimationEffect>) {
        if self.associated_effect.get().as_deref() == effect {
            return;
        }
        self.remove_sample(cx);
        if let Some(old) = self.associated_effect.get() {
            old.set_associated_animation(None);
        }
        if let Some(effect) = effect {
            if let Some(old) = effect.associated_animation() {
                old.remove_sample(cx);
                old.associated_effect.set(None);
                old.effect_changed(cx);
            }
            effect.set_associated_animation(Some(self));
        }
        self.associated_effect.set(effect);
        self.effect_changed(cx);
    }
}

impl AnimationMethods<crate::DomTypeHolder> for Animation {
    event_handler!(finish, GetOnfinish, SetOnfinish);
    event_handler!(cancel, GetOncancel, SetOncancel);
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        effect: Option<&AnimationEffect>,
        timeline: Option<Option<&AnimationTimeline>>,
    ) -> DomRoot<Self> {
        let animation = Self::new_with_proto(cx, window, proto);
        match timeline {
            Some(timeline) => animation.timeline.set(timeline),
            None => animation
                .timeline
                .set(Some(window.Document().Timeline().upcast())),
        }
        animation.set_effect(cx, effect);
        animation
    }
    fn Id(&self) -> DOMString {
        self.id.borrow().clone()
    }
    fn SetId(&self, id: DOMString) {
        *self.id.borrow_mut() = id;
    }
    fn GetEffect(&self) -> Option<DomRoot<AnimationEffect>> {
        self.associated_effect.get()
    }
    fn SetEffect(&self, cx: &mut JSContext, effect: Option<&AnimationEffect>) {
        self.set_effect(cx, effect);
    }
    fn GetTimeline(&self) -> Option<DomRoot<AnimationTimeline>> {
        self.timeline.get()
    }
    fn SetTimeline(&self, cx: &mut JSContext, timeline: Option<&AnimationTimeline>) {
        let time = self.current_time();
        self.timeline.set(timeline);
        if let Some(time) = time {
            self.set_time(time);
        }
        self.effect_changed(cx);
    }
    fn GetCurrentTime(&self) -> Option<Finite<f64>> {
        self.current_time().and_then(Finite::new)
    }
    fn SetCurrentTime(&self, cx: &mut JSContext, value: Option<Finite<f64>>) -> Fallible<()> {
        match value {
            Some(value) => {
                if self.pending.get() && self.paused.get() {
                    self.apply_pending_playback_rate();
                    self.resolve_ready(cx);
                }
                if self.PlayState() == AnimationPlayState::Idle {
                    self.paused.set(true);
                }
                self.set_time(*value);
            },
            None if self.current_time().is_some() => {
                return Err(Error::Type(
                    c"Current time cannot be unresolved for a non-idle animation".to_owned(),
                ));
            },
            None => {},
        }
        self.effect_changed(cx);
        Ok(())
    }
    fn GetStartTime(&self) -> Option<Finite<f64>> {
        self.start_time.get().and_then(Finite::new)
    }
    fn SetStartTime(&self, cx: &mut JSContext, value: Option<Finite<f64>>) {
        self.apply_pending_playback_rate();
        let previous = self.current_time();
        self.start_time.set(value.map(|value| *value));
        self.hold_time
            .set(if value.is_none() || self.playback_rate.get() == 0.0 {
                previous
            } else {
                None
            });
        self.paused.set(value.is_none());
        self.resolve_ready(cx);
        self.effect_changed(cx);
    }
    fn PlaybackRate(&self) -> Finite<f64> {
        Finite::wrap(self.playback_rate.get())
    }
    fn SetPlaybackRate(&self, cx: &mut JSContext, value: Finite<f64>) {
        self.pending_playback_rate.set(None);
        let time = self.current_time();
        self.playback_rate.set(*value);
        if let Some(time) = time {
            self.set_time(time);
        }
        self.effect_changed(cx);
    }
    fn PlayState(&self) -> AnimationPlayState {
        let time = self.current_time();
        if time.is_none() && self.start_time.get().is_none() && !self.pending.get() {
            return AnimationPlayState::Idle;
        }
        if self.paused.get() {
            return AnimationPlayState::Paused;
        }
        let rate = self.effective_playback_rate();
        if time.is_some_and(|time| {
            (rate > 0.0 && time >= self.end_time()) || (rate < 0.0 && time <= 0.0)
        }) {
            AnimationPlayState::Finished
        } else {
            AnimationPlayState::Running
        }
    }
    fn Pending(&self) -> bool {
        self.pending.get()
    }
    fn Ready(&self, cx: &mut JSContext) -> RootedPromise {
        let existing = self
            .ready_promise
            .borrow()
            .as_ref()
            .map(|promise| promise.root(cx));
        if let Some(promise) = existing {
            return promise;
        }
        let promise = Promise::new(cx, &self.global());
        *self.ready_promise.borrow_mut() = Some(promise.to_traced());
        if !self.pending.get() {
            promise.resolve_native(cx, self);
        }
        promise
    }
    fn Finished(&self, cx: &mut JSContext) -> RootedPromise {
        let existing = self
            .finished_promise
            .borrow()
            .as_ref()
            .map(|promise| promise.root(cx));
        if let Some(promise) = existing {
            return promise;
        }
        let promise = Promise::new(cx, &self.global());
        *self.finished_promise.borrow_mut() = Some(promise.to_traced());
        if self.finished_resolved.get() {
            promise.resolve_native(cx, self);
        }
        promise
    }
    fn Play(&self, cx: &mut JSContext) -> Fallible<()> {
        self.play(cx)
    }
    fn UpdatePlaybackRate(&self, cx: &mut JSContext, value: Finite<f64>) {
        let previous_state = self.PlayState();
        let time = self.current_time();
        self.pending_playback_rate.set(Some(*value));
        if self.pending.get() {
            return;
        }
        if matches!(
            previous_state,
            AnimationPlayState::Idle | AnimationPlayState::Paused
        ) || time.is_none()
        {
            self.apply_pending_playback_rate();
        } else if previous_state == AnimationPlayState::Finished {
            self.apply_pending_playback_rate();
            self.set_time(time.unwrap());
        } else {
            self.begin_pending();
        }
        self.effect_changed(cx);
    }
    fn Reverse(&self, cx: &mut JSContext) -> Fallible<()> {
        if self.timeline_time().is_none() {
            return Err(Error::InvalidState(None));
        }
        let previous_pending = self.pending_playback_rate.get();
        let rate = self.effective_playback_rate();
        let rate = if rate == 0.0 { 0.0 } else { -rate };
        self.pending_playback_rate.set(Some(rate));
        if let Err(error) = self.play(cx) {
            self.pending_playback_rate.set(previous_pending);
            return Err(error);
        }
        Ok(())
    }
    fn Pause(&self, cx: &mut JSContext) -> Fallible<()> {
        if self.paused.get() {
            return Ok(());
        }
        let time = self.current_time().unwrap_or_else(|| {
            if self.effective_playback_rate() < 0.0 {
                self.end_time()
            } else {
                0.0
            }
        });
        if !time.is_finite() {
            return Err(Error::InvalidState(None));
        }
        self.begin_pending();
        self.paused.set(true);
        if self.start_time.get().is_none() {
            self.hold_time.set(Some(time));
        }
        self.effect_changed(cx);
        Ok(())
    }
    fn Finish(&self, cx: &mut JSContext) -> Fallible<()> {
        let rate = self.effective_playback_rate();
        let end = self.end_time();
        if rate == 0.0 || (rate > 0.0 && !end.is_finite()) {
            return Err(Error::InvalidState(None));
        }
        self.apply_pending_playback_rate();
        self.paused.set(false);
        self.set_time(if rate < 0.0 { 0.0 } else { end });
        self.resolve_ready(cx);
        self.effect_changed(cx);
        self.notify_finished(cx);
        Ok(())
    }
    fn Cancel(&self, cx: &mut JSContext) {
        let was_idle = self.PlayState() == AnimationPlayState::Idle;
        if self.pending.replace(false) {
            let promise = self
                .ready_promise
                .borrow_mut()
                .take()
                .map(|promise| promise.root(cx));
            if let Some(promise) = promise {
                promise.reject_error(cx, Error::Abort(None));
                promise.set_promise_is_handled(cx);
            }
        }
        if self.PlayState() != AnimationPlayState::Idle {
            let promise = self
                .finished_promise
                .borrow_mut()
                .take()
                .map(|promise| promise.root(cx));
            if let Some(promise) = promise {
                if !self.finished_resolved.get() {
                    promise.reject_error(cx, Error::Abort(None));
                    promise.set_promise_is_handled(cx);
                }
            }
            self.finished_resolved.set(false);
        }
        if !was_idle {
            self.queue_playback_event(cx, atom!("cancel"), None);
        }
        self.apply_pending_playback_rate();
        self.start_time.set(None);
        self.hold_time.set(None);
        self.paused.set(false);
        self.remove_sample(cx);
        self.window
            .Document()
            .animation_manager()
            .track_script_animation(self, &self.window);
    }
}
