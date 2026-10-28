/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use cssparser::Parser;
use dom_struct::dom_struct;
use js::context::JSContext;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::AnimationEffectBinding::{
    AnimationEffectMethods, ComputedEffectTiming, EffectTiming, FillMode, OptionalEffectTiming,
    PlaybackDirection,
};
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::error::{Error, Fallible};
use script_bindings::num::Finite;
use script_bindings::reflector::Reflector;
use script_bindings::root::{Dom, DomRoot};
use script_bindings::str::DOMString;
use style::parser::Parse;
use style::stylesheets::CssRuleType;
use style::values::generics::easing::TimingKeyword;
use style::values::specified::TimingFunction;
use style_traits::{ParsingMode, ToCss};

use crate::css::css::parser_context_for_document;
use crate::dom::Window;
use crate::dom::animation::Animation;
use crate::dom::bindings::codegen::UnionTypes::UnrestrictedDoubleOrString;
use crate::dom::bindings::root::MutNullableDom;

/// <https://drafts.csswg.org/web-animations-1/#animationeffect>
#[dom_struct]
pub(crate) struct AnimationEffect {
    reflector: Reflector,
    associated_animation: MutNullableDom<Animation>,

    /// The window that this `AnimationEffect` was constructed in.
    window: Dom<Window>,

    specified_timing_properties: DomRefCell<SpecifiedTimingProperties>,
}

#[derive(Clone, JSTraceable, MallocSizeOf)]
struct SpecifiedTimingProperties {
    /// <https://drafts.csswg.org/web-animations-1/#start-delay>
    start_delay: Finite<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#end-delay>
    end_delay: Finite<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#fill-mode>
    fill_mode: FillMode,

    /// <https://drafts.csswg.org/web-animations-1/#iteration-count>
    iteration_count: f64,

    /// <https://drafts.csswg.org/web-animations-1/#iteration-start>
    iteration_start: Finite<f64>,

    /// <https://drafts.csswg.org/web-animations-1/#iteration-duration>
    iteration_duration: IterationDurationOrAuto,

    /// <https://drafts.csswg.org/web-animations-1/#playback-direction>
    playback_direction: PlaybackDirection,

    /// <https://drafts.csswg.org/css-easing-2/#easing-function>
    #[no_trace]
    easing_function: TimingFunction,
}

impl AnimationEffect {
    pub(crate) fn new_inherited(window: &Window) -> Self {
        Self {
            reflector: Reflector::new(),
            associated_animation: Default::default(),
            window: Dom::from_ref(window),

            // The default values of the timing properties specified here don't matter.
            // There is no way to construct a AnimationEffect without subsequently initializing them,
            // even if they're not passed to the constructor.
            specified_timing_properties: DomRefCell::new(SpecifiedTimingProperties {
                start_delay: Default::default(),
                end_delay: Default::default(),
                fill_mode: FillMode::None,
                iteration_count: Default::default(),
                iteration_start: Default::default(),
                iteration_duration: IterationDurationOrAuto::Auto,
                playback_direction: PlaybackDirection::Normal,
                easing_function: TimingFunction::Keyword(TimingKeyword::Linear),
            }),
        }
    }

    pub(crate) fn associated_animation(&self) -> Option<DomRoot<Animation>> {
        self.associated_animation.get()
    }

    pub(crate) fn set_associated_animation(&self, animation: Option<&Animation>) {
        self.associated_animation.set(animation);
    }

    pub(crate) fn active_duration(&self) -> f64 {
        let timing = self.specified_timing_properties.borrow();
        let duration = timing.iteration_duration.computed_value();
        if duration == 0.0 || timing.iteration_count == 0.0 {
            0.0
        } else {
            duration * timing.iteration_count
        }
    }

    pub(crate) fn end_time(&self) -> f64 {
        let timing = self.specified_timing_properties.borrow();
        (*timing.start_delay + self.active_duration() + *timing.end_delay).max(0.0)
    }

    pub(crate) fn progress_at(&self, local_time: Option<f64>) -> (Option<f64>, Option<f64>) {
        let Some(time) = local_time else {
            return (None, None);
        };
        let timing = self.specified_timing_properties.borrow();
        let duration = timing.iteration_duration.computed_value();
        let active = self.active_duration();
        let end = self.end_time();
        let delay = *timing.start_delay;
        let before = time < delay.min(end).max(0.0);
        let after = !before && time >= (delay + active).min(end).max(0.0);
        if (before && !matches!(timing.fill_mode, FillMode::Backwards | FillMode::Both))
            || (after && !matches!(timing.fill_mode, FillMode::Forwards | FillMode::Both))
        {
            return (None, None);
        }
        let active_time = if before {
            (time - delay).max(0.0)
        } else if after {
            (end - delay).max(0.0).min(active)
        } else {
            time - delay
        };
        let overall = *timing.iteration_start
            + if duration == 0.0 {
                if before { 0.0 } else { timing.iteration_count }
            } else {
                active_time / duration
            };
        let mut progress = if overall.is_infinite() {
            timing.iteration_start.fract()
        } else {
            overall.fract()
        };
        if progress == 0.0 && after && timing.iteration_count != 0.0 && active_time == active {
            progress = 1.0;
        }
        let iteration = if overall.is_infinite() {
            f64::INFINITY
        } else {
            overall.floor() - if progress == 1.0 { 1.0 } else { 0.0 }
        };
        let reverse = match timing.playback_direction {
            PlaybackDirection::Normal => false,
            PlaybackDirection::Reverse => true,
            PlaybackDirection::Alternate => iteration % 2.0 == 1.0,
            PlaybackDirection::Alternate_reverse => iteration % 2.0 != 1.0,
        };
        if reverse {
            progress = 1.0 - progress;
        }
        let before_flag = if before != reverse {
            style::values::generics::easing::BeforeFlag::Set
        } else {
            style::values::generics::easing::BeforeFlag::Unset
        };
        let progress = timing
            .easing_function
            .to_computed_value_without_context()
            .calculate_output(progress, before_flag, 1e-7);
        (Some(progress), Some(iteration))
    }

    pub(crate) fn window(&self) -> &Window {
        &self.window
    }

    /// <https://drafts.csswg.org/web-animations-1/#update-the-timing-properties-of-an-animation-effect>
    pub(crate) fn update_the_timing_properties(
        &self,
        input: &OptionalEffectTiming,
    ) -> Fallible<()> {
        // Step 1. If the iterationStart member of input exists and is less than zero,
        // throw a TypeError and abort this procedure.
        if input
            .iterationStart
            .is_some_and(|iteration_start| *iteration_start < 0.0)
        {
            return Err(Error::Type(
                c"Negative values for iterationStart are not allowed".to_owned(),
            ));
        }

        // Step 2. If the iterations member of input exists, and is less than zero or is the value NaN,
        // throw a TypeError and abort this procedure.
        if input
            .iterations
            .is_some_and(|iterations| iterations < 0.0 || iterations.is_nan())
        {
            return Err(Error::Type(
                c"\"iterations\" must be a positive number".to_owned(),
            ));
        }

        // Step 3. If the duration member of input exists, and is less than zero, is the value NaN,
        // or is a string other than the lowercase auto, throw a TypeError and abort this procedure.
        let Ok(duration) = input
            .duration
            .as_ref()
            .map(|duration| match duration {
                UnrestrictedDoubleOrString::UnrestrictedDouble(double) => {
                    if *double < 0.0 || double.is_nan() {
                        Err(())
                    } else {
                        Ok(IterationDurationOrAuto::Duration(*double))
                    }
                },
                UnrestrictedDoubleOrString::String(string) => {
                    if string == "auto" {
                        Ok(IterationDurationOrAuto::Auto)
                    } else {
                        Err(())
                    }
                },
            })
            .transpose()
        else {
            return Err(Error::Type(
                c"\"duration\" must be a positive number".to_owned(),
            ));
        };

        // Step 4. If the easing member of input exists but cannot be parsed using the <easing-function> production [CSS-EASING-1],
        // throw a TypeError and abort this procedure.
        let Ok(easing) = input
            .easing
            .as_ref()
            .map(|easing| {
                let easing = easing.str();
                let mut parser = Parser::new(&easing);

                // None of these values should matter
                let document = self.window.Document();
                let urlextradata = document.url().into_url().into();
                let parser_context = parser_context_for_document(
                    &document,
                    CssRuleType::Style,
                    ParsingMode::DEFAULT,
                    &urlextradata,
                );
                parser
                    .parse_entirely(|parser| TimingFunction::parse(&parser_context, parser))
                    .map_err(|_| ())
            })
            .transpose()
        else {
            return Err(Error::Type(
                c"\"easing\" is not a valid timing function".to_owned(),
            ));
        };

        // Step 5. Assign each member that exists in input to the corresponding timing property of effect as follows:
        // delay → start delay
        let mut specified_timing_properties = self.specified_timing_properties.borrow_mut();
        if let Some(start_delay) = input.delay {
            specified_timing_properties.start_delay = start_delay;
        }

        // endDelay → end delay
        if let Some(end_delay) = input.endDelay {
            specified_timing_properties.end_delay = end_delay;
        }

        // fill → fill mode
        if let Some(fill) = input.fill {
            specified_timing_properties.fill_mode = fill;
        }

        // iterationStart → iteration start
        if let Some(iteration_start) = input.iterationStart {
            specified_timing_properties.iteration_start = iteration_start;
        }

        // iterations → iteration count
        if let Some(iterations) = input.iterations {
            specified_timing_properties.iteration_count = iterations;
        }

        // duration → iteration duration
        if let Some(duration) = duration {
            specified_timing_properties.iteration_duration = duration;
        }

        // direction → playback direction
        if let Some(direction) = input.direction {
            specified_timing_properties.playback_direction = direction;
        }

        // easing → easing function
        if let Some(easing) = easing {
            specified_timing_properties.easing_function = easing;
        }
        Ok(())
    }
}

impl AnimationEffectMethods<crate::DomTypeHolder> for AnimationEffect {
    /// <https://drafts.csswg.org/web-animations-1/#dom-animationeffect-gettiming>
    fn GetTiming(&self) -> EffectTiming {
        // > Returns the specified timing properties for this animation effect.
        let specified_timing_properties = self.specified_timing_properties.borrow();
        EffectTiming {
            delay: specified_timing_properties.start_delay,
            direction: specified_timing_properties.playback_direction,
            duration: specified_timing_properties.iteration_duration.into(),
            easing: specified_timing_properties
                .easing_function
                .to_css_string()
                .into(),
            endDelay: specified_timing_properties.end_delay,
            fill: specified_timing_properties.fill_mode,
            iterationStart: specified_timing_properties.iteration_start,
            iterations: specified_timing_properties.iteration_count,
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animationeffect-getcomputedtiming>
    fn GetComputedTiming(&self) -> ComputedEffectTiming {
        // > Returns the calculated timing properties for this animation effect.
        let specified_timing_properties = self.specified_timing_properties.borrow();

        // > while getTiming() can return the string auto, getComputedTiming() must return a number
        // > corresponding to the calculated value of the iteration duration as defined in the description
        // > of the duration member of the EffectTiming interface.
        // > In this level of the specification, that simply means that an auto value is replaced by zero.
        let computed_duration = specified_timing_properties
            .iteration_duration
            .computed_value();

        // > likewise, while getTiming() can return the string auto, getComputedTiming() must return the
        // > specific FillMode used for timing calculations as defined in the description of the fill
        // > member of the EffectTiming interface.
        // > In this level of the specification, that simply means that an auto value is replaced by the none FillMode.
        let computed_fill_mode = if specified_timing_properties.fill_mode == FillMode::Auto {
            FillMode::None
        } else {
            specified_timing_properties.fill_mode
        };

        // A zero iteration duration or count produces zero active duration,
        // including when the other operand is infinite.
        let active_duration =
            if computed_duration == 0.0 || specified_timing_properties.iteration_count == 0.0 {
                0.0
            } else {
                computed_duration * specified_timing_properties.iteration_count
            };
        let end_time = (*specified_timing_properties.start_delay
            + active_duration
            + *specified_timing_properties.end_delay)
            .max(0.0);

        let local_time = self
            .associated_animation()
            .and_then(|animation| animation.current_time());
        let (progress, iteration) = self.progress_at(local_time);
        ComputedEffectTiming {
            parent: EffectTiming {
                delay: specified_timing_properties.start_delay,
                direction: specified_timing_properties.playback_direction,
                duration: UnrestrictedDoubleOrString::UnrestrictedDouble(computed_duration),
                easing: specified_timing_properties
                    .easing_function
                    .to_css_string()
                    .into(),
                endDelay: specified_timing_properties.end_delay,
                fill: computed_fill_mode,
                iterationStart: specified_timing_properties.iteration_start,
                iterations: specified_timing_properties.iteration_count,
            },
            endTime: Some(end_time),
            activeDuration: Some(active_duration),
            localTime: Some(local_time.and_then(Finite::new)),
            progress: Some(progress.and_then(Finite::new)),
            currentIteration: Some(iteration),
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-animationeffect-updatetiming>
    fn UpdateTiming(&self, cx: &mut JSContext, timing: &OptionalEffectTiming) -> Fallible<()> {
        // > Updates the specified timing properties of this animation effect by performing the procedure
        // > to update the timing properties of an animation effect passing the timing parameter as input.
        let animation = self.associated_animation();
        let previous_time = animation
            .as_ref()
            .and_then(|animation| animation.current_time());
        self.update_the_timing_properties(timing)?;
        if let Some(animation) = animation {
            animation.timing_changed(cx, previous_time);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, JSTraceable, MallocSizeOf)]
enum IterationDurationOrAuto {
    Duration(f64),
    Auto,
}

impl IterationDurationOrAuto {
    fn computed_value(&self) -> f64 {
        match self {
            IterationDurationOrAuto::Auto => 0.0,
            IterationDurationOrAuto::Duration(double) => *double,
        }
    }
}

impl From<IterationDurationOrAuto> for UnrestrictedDoubleOrString {
    fn from(value: IterationDurationOrAuto) -> Self {
        match value {
            IterationDurationOrAuto::Auto => {
                UnrestrictedDoubleOrString::String(DOMString::from_static("auto"))
            },
            IterationDurationOrAuto::Duration(double) => {
                UnrestrictedDoubleOrString::UnrestrictedDouble(double)
            },
        }
    }
}
