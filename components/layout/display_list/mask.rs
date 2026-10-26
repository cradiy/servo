/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use euclid::Size2D;
use net_traits::image_cache::{ImageMaskMode, ImageMaskTiling};
use style::values::computed::Image;
use style::values::specified::svg::MaskMode;
use webrender_api::units::LayoutRect;
use webrender_api::{ClipChainId, FillRule, ImageMask, SpatialId};

use super::{
    BackgroundPainter, BuilderForBoxFragment, DisplayListBuilder, ResolvedImage, StackingContext,
    StackingContextFragments, TraversalState, background,
};
use crate::replaced::NaturalSizes;

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

        let mask = (|| {
            // Each image-mask clip represents one image layer.
            if svg.mask_image.0.len() != 1 {
                return None;
            }
            let mode = match svg.mask_mode.0[0] {
                MaskMode::Luminance => ImageMaskMode::Luminance,
                MaskMode::Alpha | MaskMode::MatchSource => ImageMaskMode::Alpha,
            };
            let node = fragment.base.tag.map(|tag| tag.node);
            let ResolvedImage::Image { image, size, .. } = self
                .image_resolver
                .resolve_image(node, &svg.mask_image.0[0])
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
                0,
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
            let image_clip = self.wr().define_clip_image_mask(
                spatial_id,
                ImageMask {
                    image: key,
                    rect: mask_rect,
                },
                &[],
                FillRule::Nonzero,
            );
            let area_clip = self.wr().define_clip_rect(spatial_id, clip_rect);
            let parent = if layer.common.clip_chain_id == ClipChainId::INVALID {
                parent
            } else {
                Some(layer.common.clip_chain_id)
            };
            Some(self.wr().define_clip_chain(parent, [image_clip, area_clip]))
        })();

        // An unavailable mask is transparent, not an unmasked solid rectangle.
        Some(mask.unwrap_or_else(|| {
            let clip = self.wr().define_clip_rect(spatial_id, LayoutRect::zero());
            self.wr().define_clip_chain(parent, [clip])
        }))
    }
}
