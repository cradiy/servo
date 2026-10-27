/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use euclid::Size2D;
use net_traits::image_cache::{ImageMaskComposite, ImageMaskLayer, ImageMaskMode, ImageMaskTiling};
use style::computed_values::background_clip::single_value::T as Clip;
use style::values::computed::Image;
use style::values::specified::svg::{MaskComposite, MaskMode};
use webrender_api::units::LayoutRect;
use webrender_api::{ClipChainId, FillRule, ImageMask, SpatialId};

use super::{
    BackgroundPainter, BuilderForBoxFragment, DisplayListBuilder, ResolvedImage, StackingContext,
    StackingContextFragments, ToWebRender, TraversalState, background, offset_radii,
};
use crate::replaced::NaturalSizes;

struct PreparedMaskLayer {
    key: webrender_api::ImageKey,
    rect: LayoutRect,
    clip: LayoutRect,
    chain: ClipChainId,
    rounded_clip: LayoutRect,
    radii: [[f32; 2]; 4],
}

impl DisplayListBuilder<'_> {
    pub(super) fn image_mask_clip(
        &mut self,
        context: &StackingContext,
        spatial_id: SpatialId,
        parent: Option<ClipChainId>,
    ) -> Option<ClipChainId> {
        let StackingContextFragments::Fragment(fragment) = &context.fragment else {
            return parent;
        };
        let style = fragment.style();
        let svg = style.get_svg();
        if svg
            .mask_image
            .0
            .iter()
            .all(|image| matches!(image, Image::None))
        {
            return parent;
        }
        let result = (|| {
            if svg.mask_image.0.len() == 1 {
                let PreparedMaskLayer {
                    key,
                    rect,
                    clip,
                    chain,
                    ..
                } = self.mask_layer(context, 0)?;
                let mask = self.wr().define_clip_image_mask(
                    spatial_id,
                    ImageMask { image: key, rect },
                    &[],
                    FillRule::Nonzero,
                );
                let area = self.wr().define_clip_rect(spatial_id, clip);
                let parent = if chain == ClipChainId::INVALID {
                    parent
                } else {
                    Some(chain)
                };
                return Some(self.wr().define_clip_chain(parent, [mask, area]));
            }
            if svg.mask_image.0.len() > 32 {
                return None;
            }
            let mut bounds: Option<LayoutRect> = None;
            let mut prepared = Vec::new();
            for index in 0..svg.mask_image.0.len() {
                let layer = self.mask_layer(context, index);
                if let Some(layer) = &layer {
                    bounds = Some(bounds.map_or(layer.clip, |bounds| bounds.union(&layer.clip)));
                }
                prepared.push(layer);
            }
            let bounds = bounds?;
            let scale = self.device_pixel_ratio.get();
            let size = Size2D::new(
                (bounds.width() * scale).ceil(),
                (bounds.height() * scale).ceil(),
            )
            .to_i32();
            let relative = |rect: LayoutRect| {
                [
                    (rect.min.x - bounds.min.x) * scale,
                    (rect.min.y - bounds.min.y) * scale,
                    rect.width() * scale,
                    rect.height() * scale,
                ]
            };
            let layers = prepared
                .into_iter()
                .enumerate()
                .map(|(index, layer)| {
                    let composite = match svg.mask_composite.0[index % svg.mask_composite.0.len()] {
                        MaskComposite::Add => ImageMaskComposite::Add,
                        MaskComposite::Subtract => ImageMaskComposite::Subtract,
                        MaskComposite::Intersect => ImageMaskComposite::Intersect,
                        MaskComposite::Exclude => ImageMaskComposite::Exclude,
                    };
                    match layer {
                        Some(PreparedMaskLayer {
                            key,
                            rect,
                            clip,
                            rounded_clip,
                            radii,
                            ..
                        }) => ImageMaskLayer {
                            image: Some(key),
                            rect: relative(rect),
                            clip: relative(clip),
                            rounded_clip: relative(rounded_clip),
                            radii: radii.map(|r| [r[0] * scale, r[1] * scale]),
                            composite,
                        },
                        None => ImageMaskLayer {
                            image: None,
                            rect: [0.0; 4],
                            clip: [0.0; 4],
                            rounded_clip: [0.0; 4],
                            radii: [[0.0; 2]; 4],
                            composite,
                        },
                    }
                })
                .collect::<Vec<_>>();
            let key = self
                .image_resolver
                .image_cache
                .composite_mask_images(&layers, size)?;
            let rect = LayoutRect::from_origin_and_size(
                bounds.min,
                Size2D::new(size.width as f32 / scale, size.height as f32 / scale),
            );
            let mask = self.wr().define_clip_image_mask(
                spatial_id,
                ImageMask { image: key, rect },
                &[],
                FillRule::Nonzero,
            );
            let area = self.wr().define_clip_rect(spatial_id, bounds);
            Some(self.wr().define_clip_chain(parent, [mask, area]))
        })();
        Some(result.unwrap_or_else(|| {
            let clip = self.wr().define_clip_rect(spatial_id, LayoutRect::zero());
            self.wr().define_clip_chain(parent, [clip])
        }))
    }

    fn mask_layer(&mut self, context: &StackingContext, index: usize) -> Option<PreparedMaskLayer> {
        let StackingContextFragments::Fragment(fragment) = &context.fragment else {
            return None;
        };
        let style = fragment.style();
        let svg = style.get_svg();
        let mode = match svg.mask_mode.0[index % svg.mask_mode.0.len()] {
            MaskMode::Luminance => ImageMaskMode::Luminance,
            MaskMode::Alpha | MaskMode::MatchSource => ImageMaskMode::Alpha,
        };
        let node = fragment.base.tag.map(|tag| tag.node);
        let ResolvedImage::Image { image, size, .. } = self
            .image_resolver
            .resolve_image(node, &svg.mask_image.0[index])
            .ok()?
        else {
            return None;
        };
        let mut mask_style = style.clone();
        let background = servo_arc::Arc::make_mut(&mut mask_style).mutate_background();
        background.background_size.0 = svg.mask_size.0.clone();
        background.background_position_x.0 = svg.mask_position_x.0.clone();
        background.background_position_y.0 = svg.mask_position_y.0.clone();
        background.background_repeat.0 = svg.mask_repeat.0.clone();
        background.background_origin.0 = svg.mask_origin.0.clone();
        background.background_clip.0 = svg.mask_clip.0.clone();
        background.background_attachment =
            style::properties::longhands::background_attachment::get_initial_value();
        background.background_blend_mode =
            style::properties::longhands::background_blend_mode::get_initial_value();
        let fragment = fragment.with_style();
        let fragment_builder =
            BuilderForBoxFragment::new(&fragment, context.containing_block_origin);
        let painter = BackgroundPainter {
            style: &mask_style,
            positioning_area_override: None,
            painting_area_override: None,
        };
        let state = TraversalState {
            spatial_id: context.scroll_tree_node_id,
            clip_id: context.clip_id,
            origin: context.containing_block_origin,
            ..Default::default()
        };
        let layer = background::layout_layer(
            &fragment_builder,
            &painter,
            self,
            &state,
            index,
            NaturalSizes::from_width_and_height(size.width, size.height),
        )?;
        let scale = self.device_pixel_ratio.get();
        let raster_size = Size2D::new(
            layer.tile_size.width * scale,
            layer.tile_size.height * scale,
        )
        .to_i32();
        let clip_rect = layer.bounds.intersection(&layer.common.clip_rect)?;
        let (mask_rect, tiling) = if layer.repeat {
            let size = Size2D::new(
                (clip_rect.width() * scale).ceil(),
                (clip_rect.height() * scale).ceil(),
            )
            .to_i32();
            let tiling = ImageMaskTiling {
                size,
                tile_size: [
                    layer.tile_size.width * scale,
                    layer.tile_size.height * scale,
                ],
                spacing: [
                    layer.tile_spacing.width * scale,
                    layer.tile_spacing.height * scale,
                ],
                offset: [
                    (clip_rect.min.x - layer.bounds.min.x) * scale,
                    (clip_rect.min.y - layer.bounds.min.y) * scale,
                ],
            };
            (
                LayoutRect::from_origin_and_size(
                    clip_rect.min,
                    Size2D::new(size.width as f32 / scale, size.height as f32 / scale),
                ),
                Some(tiling),
            )
        } else {
            (
                LayoutRect::from_origin_and_size(layer.bounds.min, layer.tile_size),
                None,
            )
        };
        let key = self.image_resolver.mask_key_from_cached_image(
            &image,
            raster_size,
            node,
            mode,
            tiling,
        )?;

        let (rounded_clip, radii) = match svg.mask_clip.0[index % svg.mask_clip.0.len()] {
            Clip::ContentBox => (
                *fragment_builder.content_rect(),
                offset_radii(
                    fragment_builder.border_radius(),
                    -(fragment.border + fragment.padding).to_webrender(),
                ),
            ),
            Clip::PaddingBox => (
                *fragment_builder.padding_rect(),
                offset_radii(
                    fragment_builder.border_radius(),
                    -fragment.border.to_webrender(),
                ),
            ),
            Clip::BorderBox | Clip::BorderArea => (
                fragment_builder.border_rect,
                fragment_builder.border_radius(),
            ),
        };
        let radii = [
            radii.top_left,
            radii.top_right,
            radii.bottom_right,
            radii.bottom_left,
        ]
        .map(|r| [r.width, r.height]);
        Some(PreparedMaskLayer {
            key,
            rect: mask_rect,
            clip: clip_rect,
            chain: layer.common.clip_chain_id,
            rounded_clip,
            radii,
        })
    }
}
