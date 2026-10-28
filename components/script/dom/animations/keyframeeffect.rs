/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::Cell;
use std::ffi::CString;
use std::ops::ControlFlow;
use std::ptr::NonNull;
use std::sync::LazyLock;

use cssparser::Parser;
use dom_struct::dom_struct;
use js::context::JSContext;
use js::conversions::{
    ConversionResult, FromJSValConvertible, ToJSValConvertible, jsstr_to_string,
};
use js::gc::{HandleValue, RootedVec};
use js::jsapi::{Heap, JS_GetPropertyById, JSITER_OWNONLY, JSObject};
use js::jsval::{ObjectValue, UndefinedValue};
use js::rust::{
    ForOfIterationFailure, HandleObject, IdVector, IntoHandle, IntoMutableHandle, for_of,
};
use rustc_hash::FxHashMap;
use script_bindings::cell::DomRefCell;
use script_bindings::codegen::GenericBindings::AnimationEffectBinding::{
    EffectTiming, OptionalEffectTiming,
};
use script_bindings::codegen::GenericBindings::KeyframeEffectBinding::{
    BaseKeyframe, BasePropertyIndexedKeyframe, CompositeOperation, CompositeOperationOrAuto,
};
use script_bindings::codegen::GenericBindings::WindowBinding::WindowMethods;
use script_bindings::codegen::GenericUnionTypes::UnrestrictedDoubleOrKeyframeEffectOptions;
use script_bindings::codegen::GenericUnionTypes::{
    CompositeOperationOrAutoOrCompositeOperationOrAutoSequence as CompositeKeyframeValues,
    DoubleOrDoubleOrNullSequence, StringOrStringSequence, UnrestrictedDoubleOrString,
};
use script_bindings::conversions::StringificationBehavior;
use script_bindings::error::{Error, Fallible};
use script_bindings::inheritance::Castable;
use script_bindings::js::{
    GetPropertyKeys, JS_DefineProperty, JS_IdToValue, JS_NewObject, PropertyFlags,
};
use script_bindings::num::Finite;
use script_bindings::reflector::reflect_dom_object_with_proto;
use script_bindings::root::DomRoot;
use script_bindings::str::DOMString;
use style::parser::Parse;
use style::parser::ParserContext;
use style::properties::generated::PropertyDeclaration;
use style::properties::{
    Importance, NonCustomPropertyId, PropertyDeclarationBlock, PropertyId,
    SourcePropertyDeclaration,
};
use style::stylesheets::CssRuleType;
use style::values::specified::TimingFunction;
use style_traits::{CssWriter, ParsingMode, ToCss};

use crate::css::css::parser_context_for_document;
use crate::dom::Document;
use crate::dom::animationeffect::AnimationEffect;
use crate::dom::bindings::codegen::Bindings::KeyframeEffectBinding::{
    BaseComputedKeyframe, KeyframeEffectMethods,
};
use crate::dom::bindings::root::MutNullableDom;
use crate::dom::element::Element;
use crate::dom::window::Window;

/// <https://drafts.csswg.org/web-animations-1/#keyframeeffect>
#[dom_struct]
pub(crate) struct KeyframeEffect {
    animationeffect: AnimationEffect,

    /// <https://drafts.csswg.org/web-animations-1/#effect-target-target-element>
    // FIXME: Store a target pseudo-selector
    // to fully match the concept of the effect target
    //
    // https://drafts.csswg.org/web-animations-1/#effect-target-target-pseudo-selector
    // https://drafts.csswg.org/web-animations-1/#keyframe-effect-effect-target.
    target_element: MutNullableDom<Element>,
    composite: Cell<CompositeOperation>,

    /// <https://drafts.csswg.org/web-animations-1/#keyframe>
    keyframes: DomRefCell<Vec<Keyframe>>,
}

impl KeyframeEffect {
    pub(crate) fn target(&self) -> Option<DomRoot<Element>> {
        self.target_element.get()
    }

    pub(crate) fn sample_values(
        &self,
        target: &Element,
        progress: f64,
    ) -> style::properties::animated_properties::AnimationValueMap {
        use crate::dom::node::{Node, NodeTraits};
        use style::properties::animated_properties::{AnimationValue, AnimationValueMap};
        use style::values::animated::{Animate, Procedure};
        let (blocks, offsets, easings) = {
            let frames = self.keyframes.borrow();
            let blocks = frames
                .iter()
                .map(|frame| {
                    let mut block = PropertyDeclarationBlock::new();
                    for declaration in &frame.declarations {
                        for value in declaration.block.declarations() {
                            block.push(value.clone(), Importance::Normal);
                        }
                    }
                    block
                })
                .collect::<Vec<_>>();
            (
                blocks,
                computed_offsets(&frames),
                frames
                    .iter()
                    .map(|frame| frame.easing_function.clone())
                    .collect::<Vec<_>>(),
            )
        };
        if blocks.is_empty() {
            return AnimationValueMap::default();
        }
        let window = target.owner_window();
        let Some((underlying, computed)) =
            window.animation_values_query(target.upcast::<Node>(), &blocks)
        else {
            return AnimationValueMap::default();
        };
        let document = window.Document();
        let url = document.url().into_url().into();
        let context =
            parser_context_for_document(&document, CssRuleType::Style, ParsingMode::DEFAULT, &url);
        let mut result = AnimationValueMap::default();
        for (property, base) in underlying {
            let mut track: Vec<(f64, AnimationValue, usize)> = computed
                .iter()
                .enumerate()
                .filter_map(|(index, values)| {
                    values
                        .get(&property)
                        .map(|value| (offsets[index], value.clone(), index))
                })
                .collect();
            if track.is_empty() {
                continue;
            }
            if track[0].0 > 0.0 {
                track.insert(0, (0.0, base.clone(), usize::MAX));
            }
            if track.last().unwrap().0 < 1.0 {
                track.push((1.0, base.clone(), usize::MAX));
            }
            if track.len() == 1 {
                result.insert(property, track[0].1.clone());
                continue;
            }
            let right = track
                .iter()
                .position(|frame| frame.0 > progress)
                .unwrap_or(track.len() - 1)
                .max(1);
            let left = right - 1;
            let span = track[right].0 - track[left].0;
            let mut portion = if span == 0.0 {
                1.0
            } else {
                (progress - track[left].0) / span
            };
            if let Some(easing) = easings.get(track[left].2) {
                let source = easing.str();
                if let Ok(easing) = Parser::new(&source)
                    .parse_entirely(|parser| TimingFunction::parse(&context, parser))
                {
                    portion = easing.to_computed_value_without_context().calculate_output(
                        portion,
                        style::values::generics::easing::BeforeFlag::Unset,
                        1e-7,
                    );
                }
            }
            let value = track[left]
                .1
                .animate(
                    &track[right].1,
                    Procedure::Interpolate { progress: portion },
                )
                .unwrap_or_else(|_| {
                    if portion < 0.5 {
                        track[left].1.clone()
                    } else {
                        track[right].1.clone()
                    }
                });
            result.insert(property, value);
        }
        result
    }

    pub(crate) fn new_inherited(window: &Window) -> Self {
        Self {
            animationeffect: AnimationEffect::new_inherited(window),
            target_element: Default::default(),
            composite: Cell::new(CompositeOperation::Replace),
            keyframes: Default::default(),
        }
    }

    fn new_with_proto(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
    ) -> DomRoot<Self> {
        reflect_dom_object_with_proto(cx, Box::new(Self::new_inherited(window)), window, proto)
    }
}

impl KeyframeEffectMethods<crate::DomTypeHolder> for KeyframeEffect {
    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-keyframeeffect>
    fn Constructor(
        cx: &mut JSContext,
        window: &Window,
        proto: Option<HandleObject>,
        target: Option<&Element>,
        keyframes: *mut JSObject,
        options: UnrestrictedDoubleOrKeyframeEffectOptions,
    ) -> Fallible<DomRoot<KeyframeEffect>> {
        // Step 1. Create a new KeyframeEffect object, effect.
        let effect = KeyframeEffect::new_with_proto(cx, window, proto);

        // Step 2. Set the target element of effect to target.
        effect.target_element.set(target);

        // TODO: Step 3. Set the target pseudo-selector to the result corresponding to
        // the first matching condition below:

        let timing = match options {
            UnrestrictedDoubleOrKeyframeEffectOptions::UnrestrictedDouble(duration) => {
                EffectTiming {
                    duration: UnrestrictedDoubleOrString::UnrestrictedDouble(duration),
                    ..Default::default()
                }
            },
            UnrestrictedDoubleOrKeyframeEffectOptions::KeyframeEffectOptions(options) => {
                effect.composite.set(options.composite);
                options.parent
            },
        };

        // Step 5. Call the procedure to update the timing properties of an animation effect of
        // effect from timing input.
        // If that procedure causes an exception to be thrown, propagate the exception and abort this procedure.
        effect
            .upcast::<AnimationEffect>()
            .update_the_timing_properties(&OptionalEffectTiming {
                delay: Some(timing.delay),
                endDelay: Some(timing.endDelay),
                fill: Some(timing.fill),
                iterationStart: Some(timing.iterationStart),
                iterations: Some(timing.iterations),
                duration: Some(timing.duration),
                direction: Some(timing.direction),
                easing: Some(timing.easing),
            })?;

        // Step 7. Initialize the set of keyframes by performing the procedure defined for setKeyframes()
        // passing keyframes as the input.
        effect.SetKeyframes(cx, keyframes)?;

        Ok(effect)
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-target>
    fn GetTarget(&self) -> Option<DomRoot<Element>> {
        self.target_element.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-target>
    fn SetTarget(&self, cx: &mut JSContext, target: Option<&Element>) {
        self.target_element.set(target);
        if let Some(animation) = self.animationeffect.associated_animation() {
            animation.effect_changed(cx);
        }
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-composite>
    fn Composite(&self) -> CompositeOperation {
        self.composite.get()
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-composite>
    fn SetComposite(&self, composite: CompositeOperation) {
        self.composite.set(composite);
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-getkeyframes>
    fn GetKeyframes(
        &self,
        cx: &mut JSContext,
        result: &mut RootedVec<'_, Box<Heap<*mut JSObject>>>,
    ) -> Fallible<()> {
        let window = self.upcast::<AnimationEffect>().window();

        // Step 1. Let result be an empty sequence of objects.
        debug_assert!(result.is_empty());

        // Step 2. Let keyframes be one of the following:
        // If this keyframe effect is associated with a CSSAnimation, and its keyframes have not been replaced
        // by a successful call to setKeyframes(),
        // the computed keyframes for this keyframe effect.
        // Otherwise,
        // the result of applying the procedure compute missing keyframe offsets to the keyframes for this keyframe effect.
        let keyframes = self.keyframes.borrow();

        // Step 3. For each keyframe in keyframes perform the following steps:
        let offsets = computed_offsets(&keyframes);
        for (keyframe, computed_offset) in keyframes.iter().zip(offsets) {
            // Step 3.1 Initialize a dictionary object, output keyframe, using the following definition:
            // Step 3.2 Set the offset, computedOffset, easing, and composite members of output keyframe
            // to the respective keyframe offset, computed keyframe offset, keyframe-specific easing function,
            // and keyframe-specific composite operation values of keyframe.
            let base_keyframe = BaseComputedKeyframe {
                composite: keyframe.composite,
                offset: keyframe.offset,
                computedOffset: Some(Finite::wrap(computed_offset)),
                easing: keyframe.easing_function.clone(),
            };
            rooted!(&in(cx) let mut output_keyframe = JS_NewObject(cx, None));
            base_keyframe.to_jsobject(cx, output_keyframe.handle_mut());

            // Step 3.3 For each animation property-value pair declaration in keyframe, perform the following steps:
            for property_value_pair in &keyframe.declarations {
                debug_assert!(property_value_pair.property_id.is_animatable());

                // Step 3.3.1 Let property name be the result of applying the animation property name to IDL attribute
                // name algorithm to the property name of declaration.
                let mut property_name = String::new();
                let mut writer = CssWriter::new(&mut property_name);
                if property_value_pair.property_id.to_css(&mut writer).is_err() {
                    continue;
                }
                let property_name = animation_property_name_to_idl_attribute_name(&property_name);

                // Step 3.3.2 Let IDL value be the result of serializing the property value of declaration
                // by passing declaration to the algorithm to serialize a CSS value [CSSOM].
                let mut value_string = String::new();
                let is_single_value_value_to_css = {
                    let mut layout = window.layout_mut();
                    property_value_pair.block.single_value_to_css(
                        &property_value_pair.property_id,
                        &mut value_string,
                        None,
                        layout.stylist_mut(),
                    )
                };
                if is_single_value_value_to_css.is_err() {
                    continue;
                }

                // Step 3.3.3 Let value be the result of converting IDL value to an ECMAScript String value.
                rooted!(&in(cx) let mut value = UndefinedValue());
                value_string.to_jsval(cx, value.handle_mut());

                // Step 3.3.4 Call the [[DefineOwnProperty]] internal method on output keyframe with property
                // name property name, Property Descriptor { [[Writable]]: true, [[Enumerable]]: true, [[Configurable]]:
                // true, [[Value]]: value } and Boolean flag false.
                let Ok(property_name) = CString::new(property_name) else {
                    continue;
                };

                let success = JS_DefineProperty(
                    cx,
                    output_keyframe.handle(),
                    &property_name,
                    value.handle(),
                    PropertyFlags::ENUMERABLE
                        | PropertyFlags::WRITABLE
                        | PropertyFlags::CONFIGURABLE,
                );
                if !success {
                    if cfg!(debug_assertions) {
                        unreachable!("Setting a property on output_keyframe should never fail");
                    }
                    return Err(Error::Operation(None));
                }
            }

            // Step 3.4 Append output keyframe to result.
            result.push(Heap::boxed(output_keyframe.get()))
        }

        // Step 4. Return result.
        Ok(())
    }

    /// <https://drafts.csswg.org/web-animations-1/#dom-keyframeeffect-setkeyframes>
    fn SetKeyframes(&self, cx: &mut JSContext, keyframes: *mut JSObject) -> Fallible<()> {
        // > This effect’s set of keyframes is replaced with the result of performing the procedure to
        // > process a keyframes argument. If that procedure throws an exception, this effect’s
        // > keyframes are not modified.
        let document = self.upcast::<AnimationEffect>().window().Document();
        let keyframes = process_a_keyframes_argument(cx, &document, keyframes)?;
        *self.keyframes.safe_borrow_mut(cx.no_gc()) = keyframes;
        if let Some(animation) = self.animationeffect.associated_animation() {
            animation.effect_changed(cx);
        }
        Ok(())
    }
}

/// <https://drafts.csswg.org/web-animations-1/#process-a-keyframes-argument>
fn process_a_keyframes_argument(
    cx: &mut JSContext,
    document: &Document,
    keyframes: *mut JSObject,
) -> Fallible<Vec<Keyframe>> {
    // Step 1. If object is null, return an empty sequence of keyframes.
    if keyframes.is_null() {
        return Ok(Vec::new());
    }

    // Step 2. Let processed keyframes be an empty sequence of keyframes.

    // Step 3. Let method be the result of GetMethod(object, @@iterator).
    // Step 4. Check the completion record of method.
    // Step 5. Perform the steps corresponding to the first matching condition below:
    rooted!(&in(cx) let iterable = ObjectValue(keyframes));
    let mut keyframes = Vec::new();
    let result = for_of(cx, iterable.handle(), |cx, iterator_element| {
        // Step 5.3.4 Let nextItem be IteratorValue(next).
        // Step 5.3.5 Check the completion record of nextItem.
        // Note: This happens inside the "for_of" call.

        // Step 5.3.6 If Type(nextItem) is not Undefined, Null or Object, then throw a TypeError
        // and abort these steps.
        if !iterator_element.is_null_or_undefined() && !iterator_element.is_object() {
            return Err(ForOfIterationFailure::Other(Error::Type(
                c"Keyframe must be an object, null or undefined".to_owned(),
            )));
        }

        // Step 5.3.7 Append to processed keyframes the result of running the procedure to process a
        // keyframe-like object passing nextItem as the keyframe input with the allow lists flag set to false.
        keyframes.push(process_a_keyframe_like_object(
            cx,
            document,
            iterator_element,
        )?);

        Ok(ControlFlow::Continue(()))
    });
    let mut keyframes = match result {
        Ok(()) => Ok(keyframes),
        Err(ForOfIterationFailure::ValueIsNotIterable) => {
            process_property_indexed_keyframes(cx, document, iterable.handle())
        },
        Err(ForOfIterationFailure::JSFailed) => Err(Error::JSFailed),
        Err(ForOfIterationFailure::Other(error)) => Err(error),
    }?;
    let mut previous_offset = 0.0;
    let urlextradata = document.url().into_url().into();
    let context = parser_context_for_document(
        document,
        CssRuleType::Style,
        ParsingMode::DEFAULT,
        &urlextradata,
    );
    for keyframe in &mut keyframes {
        if let Some(offset) = keyframe.offset {
            if *offset < previous_offset || *offset > 1.0 {
                return Err(Error::Type(
                    c"Keyframe offsets must be ordered and between zero and one".to_owned(),
                ));
            }
            previous_offset = *offset;
        }
        let normalized = {
            let easing = keyframe.easing_function.str();
            Parser::new(&easing)
                .parse_entirely(|parser| TimingFunction::parse(&context, parser))
                .map_err(|_| Error::Type(c"Invalid keyframe easing".to_owned()))?
                .to_css_string()
                .into()
        };
        keyframe.easing_function = normalized;
    }
    Ok(keyframes)
}

/// <https://drafts.csswg.org/web-animations-1/#processing-a-keyframes-argument>
fn process_property_indexed_keyframes(
    cx: &mut JSContext,
    document: &Document,
    input: HandleValue,
) -> Fallible<Vec<Keyframe>> {
    let metadata = match BasePropertyIndexedKeyframe::from_jsval(cx, input, ()) {
        Ok(ConversionResult::Success(value)) => value,
        Ok(ConversionResult::Failure(message)) => return Err(Error::Type(message.into_owned())),
        Err(_) => return Err(Error::JSFailed),
    };
    rooted!(&in(cx) let object = input.to_object());
    let properties = get_property_values(cx, object.handle(), true)?;
    let urlextradata = document.url().into_url().into();
    let mut context = parser_context_for_document(
        document,
        CssRuleType::Style,
        ParsingMode::DEFAULT,
        &urlextradata,
    );
    let mut spaced_frames = Vec::new();
    for (property, values) in properties {
        let length = values.len();
        for (index, value) in values.into_iter().enumerate() {
            let offset = if length == 1 {
                1.0
            } else {
                index as f64 / (length - 1) as f64
            };
            let declaration =
                parse_single_property_declaration(&property, &value.str(), &mut context);
            spaced_frames.push((offset, declaration));
        }
    }
    spaced_frames.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut frames: Vec<Keyframe> = Vec::new();
    let mut previous_offset = None;
    for (offset, declaration) in spaced_frames {
        if previous_offset != Some(offset) {
            frames.push(Keyframe {
                offset: None,
                easing_function: DOMString::from_static("linear"),
                composite: CompositeOperationOrAuto::Auto,
                declarations: Vec::new(),
            });
            previous_offset = Some(offset);
        }
        if let Some(declaration) = declaration {
            frames.last_mut().unwrap().declarations.push(declaration);
        }
    }
    let offsets = match metadata.offset.flatten() {
        None => vec![None],
        Some(DoubleOrDoubleOrNullSequence::Double(value)) => vec![Some(value)],
        Some(DoubleOrDoubleOrNullSequence::DoubleOrNullSequence(values)) => values,
    };
    for (frame, offset) in frames.iter_mut().zip(offsets) {
        frame.offset = offset;
    }
    let mut easings = match metadata
        .easing
        .unwrap_or(StringOrStringSequence::StringSequence(Vec::new()))
    {
        StringOrStringSequence::String(value) => vec![value],
        StringOrStringSequence::StringSequence(values) => values,
    };
    if easings.is_empty() {
        easings.push(DOMString::from_static("linear"));
    }
    // Excess easing entries must be validated, even when there are no frames.
    for easing in &mut easings {
        let normalized = {
            let source = easing.str();
            Parser::new(&source)
                .parse_entirely(|parser| TimingFunction::parse(&context, parser))
                .map_err(|_| Error::Type(c"Invalid keyframe easing".to_owned()))?
                .to_css_string()
                .into()
        };
        *easing = normalized;
    }
    let composites = match metadata.composite.unwrap_or(
        CompositeKeyframeValues::CompositeOperationOrAutoSequence(Vec::new()),
    ) {
        CompositeKeyframeValues::CompositeOperationOrAuto(value) => vec![value],
        CompositeKeyframeValues::CompositeOperationOrAutoSequence(values) => values,
    };
    for (index, frame) in frames.iter_mut().enumerate() {
        frame.easing_function = easings[index % easings.len()].clone();
        if !composites.is_empty() {
            frame.composite = composites[index % composites.len()];
        }
    }
    Ok(frames)
}

fn computed_offsets(keyframes: &[Keyframe]) -> Vec<f64> {
    let mut offsets: Vec<_> = keyframes
        .iter()
        .map(|frame| frame.offset.map(|offset| *offset))
        .collect();
    if let Some(last) = offsets.last_mut() {
        last.get_or_insert(1.0);
    }
    if let Some(first) = offsets.first_mut() {
        first.get_or_insert(0.0);
    }
    let mut previous = 0;
    for index in 1..offsets.len() {
        if let Some(end) = offsets[index] {
            let start = offsets[previous].unwrap();
            for current in previous + 1..index {
                offsets[current] = Some(
                    start + (end - start) * (current - previous) as f64 / (index - previous) as f64,
                );
            }
            previous = index;
        }
    }
    offsets.into_iter().map(Option::unwrap).collect()
}

/// <https://drafts.csswg.org/web-animations-1/#keyframe>
#[derive(JSTraceable, MallocSizeOf)]
struct Keyframe {
    offset: Option<Finite<f64>>,
    easing_function: DOMString,
    composite: CompositeOperationOrAuto,
    declarations: Vec<KeyframePropertyDeclaration>,
}

#[derive(JSTraceable, MallocSizeOf)]
struct KeyframePropertyDeclaration {
    #[no_trace]
    property_id: PropertyId,
    /// The block is known to only contain declarations for `property_id`. There might
    /// be more than one value if `property_id` is a shorthand.
    #[no_trace]
    block: PropertyDeclarationBlock,
}

/// <https://drafts.csswg.org/web-animations-1/#process-a-keyframe-like-object>
fn process_a_keyframe_like_object(
    cx: &mut JSContext,
    document: &Document,
    keyframe_input: HandleValue,
) -> Fallible<Keyframe> {
    let Ok(keyframe_output) = BaseKeyframe::from_jsval(cx, keyframe_input, ()) else {
        return Err(Error::JSFailed);
    };
    let ConversionResult::Success(keyframe_output) = keyframe_output else {
        return Err(Error::Operation(None));
    };
    let mut keyframe_output = Keyframe {
        offset: keyframe_output.offset,
        easing_function: keyframe_output.easing,
        composite: keyframe_output.composite,
        declarations: Vec::new(),
    };

    // Step 2. If keyframe input is null or undefined, return keyframe output.
    if keyframe_input.is_null_or_undefined() {
        return Ok(keyframe_output);
    }

    // From Step 3 onwards our implementation diverges from the specification. The spec
    // wants us to build a list of animatable CSS properties and a list of properties on
    // the object, then compute the union between the two.
    //
    // Instead, we iterate over all properties on the object and then check if they correspond
    // to an animatable property.
    let urlextradata = document.url().into_url().into();
    let mut parser_context = parser_context_for_document(
        document,
        CssRuleType::Style,
        ParsingMode::DEFAULT,
        &urlextradata,
    );
    rooted!(&in(cx) let object = keyframe_input.to_object());

    // Read properties before parsing their CSS values.
    keyframe_output.declarations = get_property_values(cx, object.handle(), false)?
        .into_iter()
        .filter_map(|(property, values)| {
            parse_single_property_declaration(&property, &values[0].str(), &mut parser_context)
        })
        .collect();

    // Step 7. Return keyframe output.
    Ok(keyframe_output)
}

/// Reads animatable own properties in Unicode codepoint order.
#[expect(unsafe_code)]
fn get_property_values(
    cx: &mut JSContext,
    object: HandleObject,
    allow_lists: bool,
) -> Fallible<Vec<(PropertyId, Vec<DOMString>)>> {
    let mut ids = IdVector::new(cx);
    if !GetPropertyKeys(cx, object, JSITER_OWNONLY, ids.handle_mut()) {
        return Err(Error::JSFailed);
    }
    let mut properties = Vec::new();
    for (index, id) in ids.iter().enumerate() {
        rooted!(&in(cx) let id = *id);
        if !id.is_string() {
            continue;
        }
        rooted!(&in(cx) let mut key = UndefinedValue());
        if !JS_IdToValue(cx, id.handle().get(), key.handle_mut()) {
            return Err(Error::JSFailed);
        }
        rooted!(&in(cx) let string = key.to_string());
        let Some(string) = NonNull::new(string.get()) else {
            continue;
        };
        let name = unsafe { jsstr_to_string(cx, string) };
        if let Some(property) = lookup_css_property_by_idl_attribute_name(&name) {
            properties.push((name, index, property));
        }
    }
    properties.sort_by(|a, b| a.0.cmp(&b.0));
    let mut result = Vec::with_capacity(properties.len());
    for (_, index, property) in properties {
        rooted!(&in(cx) let id = ids[index]);
        rooted!(&in(cx) let mut value = UndefinedValue());
        if !unsafe {
            JS_GetPropertyById(
                cx.raw_cx(),
                object.into_handle(),
                id.handle().into_handle(),
                value.handle_mut().into_handle_mut(),
            )
        } {
            return Err(Error::JSFailed);
        }
        let values = if allow_lists {
            match StringOrStringSequence::from_jsval(cx, value.handle(), ()) {
                Ok(ConversionResult::Success(StringOrStringSequence::String(value))) => vec![value],
                Ok(ConversionResult::Success(StringOrStringSequence::StringSequence(values))) => {
                    values
                },
                Ok(ConversionResult::Failure(message)) => {
                    return Err(Error::Type(message.into_owned()));
                },
                Err(_) => return Err(Error::JSFailed),
            }
        } else {
            match DOMString::from_jsval(cx, value.handle(), StringificationBehavior::Default) {
                Ok(ConversionResult::Success(value)) => vec![value],
                Ok(ConversionResult::Failure(message)) => {
                    return Err(Error::Type(message.into_owned()));
                },
                Err(_) => return Err(Error::JSFailed),
            }
        };
        result.push((property, values));
    }
    Ok(result)
}

/// Parses `input` as a value for `property`, returning `None` on failure.
fn parse_single_property_declaration(
    property: &PropertyId,
    input: &str,
    parser_context: &mut ParserContext<'_>,
) -> Option<KeyframePropertyDeclaration> {
    let mut declaration = SourcePropertyDeclaration::default();
    let mut parser = Parser::new(input);

    // TODO: Consider reporting parse errors somewhere useful, like the devtools console.
    parser
        .parse_entirely(|parser| {
            PropertyDeclaration::parse_into(
                &mut declaration,
                property.clone(),
                parser_context,
                parser,
            )
        })
        .ok()?;

    let mut block = PropertyDeclarationBlock::new();
    block.extend(declaration.drain(), Importance::Normal);

    Some(KeyframePropertyDeclaration {
        property_id: property.clone(),
        block,
    })
}

/// This is the inverse of <https://drafts.csswg.org/web-animations-1/#animation-property-name-to-idl-attribute-name>.
fn lookup_css_property_by_idl_attribute_name(attribute_name: &str) -> Option<PropertyId> {
    if attribute_name.starts_with("--") {
        return PropertyId::parse_enabled_for_all_content(attribute_name).ok();
    }

    // Step 4. Otherwise, return the result of applying the CSS property to IDL attribute algorithm
    // [CSSOM] to property.
    static IDL_ATTRIBUTE_TO_ANIMATED_PROPERTY_LOOKUP_TABLE: LazyLock<
        FxHashMap<String, NonCustomPropertyId>,
    > = LazyLock::new(|| {
        log::debug!("Initializing map from IDL attribute names to CSS properties");

        NonCustomPropertyId::iter()
            .filter(|non_custom_property| non_custom_property.is_animatable())
            .filter(|non_custom_property| {
                non_custom_property
                    .to_property_id()
                    .enabled_for_all_content()
            })
            .map(|non_custom_property| {
                let idl_attribute_name =
                    animation_property_name_to_idl_attribute_name(non_custom_property.name());
                (idl_attribute_name, non_custom_property)
            })
            .collect()
    });
    IDL_ATTRIBUTE_TO_ANIMATED_PROPERTY_LOOKUP_TABLE
        .get(attribute_name)
        .copied()
        .map(PropertyId::NonCustom)
}

/// <https://drafts.csswg.org/web-animations-1/#animation-property-name-to-idl-attribute-name>
///
/// # Panics
/// Panics when the property name is empty or consists only of `-` characters.
/// In general it is assumed that `property_name` is a CSS property.
fn animation_property_name_to_idl_attribute_name(property_name: &str) -> String {
    if property_name.starts_with("--") {
        return property_name.to_owned();
    }
    match property_name {
        "float" => return "cssFloat".to_owned(),
        "offset" => return "cssOffset".to_owned(),
        _ => {},
    }
    let mut idl_attribute_name = String::with_capacity(property_name.len());

    let mut chunks = property_name.split('-');
    let Some(first_chunk) = chunks.next() else {
        unreachable!("CSS property name should not consist only of dashes");
    };
    idl_attribute_name.push_str(first_chunk);
    for chunk in chunks {
        let mut characters = chunk.chars();
        let Some(to_capitalize) = characters.next() else {
            continue;
        };
        idl_attribute_name.push(to_capitalize.to_ascii_uppercase());
        idl_attribute_name.push_str(characters.as_str());
    }

    idl_attribute_name
}
