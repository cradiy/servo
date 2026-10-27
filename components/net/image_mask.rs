/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::sync::Arc;

use net_traits::image_cache::{ImageMaskComposite, ImageMaskLayer, ImageMaskTiling};
use rustc_hash::{FxHashMap, FxHashSet};
use webrender_api::{ImageKey, units::DeviceIntSize};

#[derive(Clone, Copy, Eq, Hash, PartialEq, malloc_size_of_derive::MallocSizeOf)]
pub(super) struct TilingKey(#[ignore_malloc_size_of = "Inline geometry"] [u32; 8]);

pub(super) fn tiling_key(tiling: ImageMaskTiling) -> Option<TilingKey> {
    if tiling.size.width <= 0 ||
        tiling.size.height <= 0 ||
        tiling.size.width > 8192 ||
        tiling.size.height > 8192
    {
        return None;
    }
    for axis in 0..2 {
        if !tiling.tile_size[axis].is_finite() ||
            tiling.tile_size[axis] <= 0.0 ||
            !tiling.spacing[axis].is_finite() ||
            tiling.spacing[axis] < 0.0 ||
            !(tiling.tile_size[axis] + tiling.spacing[axis]).is_finite() ||
            !tiling.offset[axis].is_finite()
        {
            return None;
        }
    }
    Some(TilingKey([
        tiling.size.width as u32,
        tiling.size.height as u32,
        tiling.tile_size[0].to_bits(),
        tiling.tile_size[1].to_bits(),
        tiling.spacing[0].to_bits(),
        tiling.spacing[1].to_bits(),
        tiling.offset[0].to_bits(),
        tiling.offset[1].to_bits(),
    ]))
}

fn samples(
    count: i32,
    tile: f32,
    spacing: f32,
    offset: f32,
    source: usize,
) -> Vec<Option<(usize, usize, f32)>> {
    let stride = tile + spacing;
    (0..count)
        .map(|index| {
            let position = (index as f32 + 0.5 + offset).rem_euclid(stride);
            if position >= tile {
                return None;
            }
            let coordinate =
                (position / tile * source as f32 - 0.5).clamp(0.0, (source - 1) as f32);
            let lower = coordinate.floor() as usize;
            Some((lower, (lower + 1).min(source - 1), coordinate.fract()))
        })
        .collect()
}

pub(super) fn tile_mask(
    source: &[u8],
    width: usize,
    height: usize,
    tiling: ImageMaskTiling,
) -> Vec<u8> {
    let x_samples = samples(
        tiling.size.width,
        tiling.tile_size[0],
        tiling.spacing[0],
        tiling.offset[0],
        width,
    );
    let y_samples = samples(
        tiling.size.height,
        tiling.tile_size[1],
        tiling.spacing[1],
        tiling.offset[1],
        height,
    );
    let mut output = vec![0; tiling.size.width as usize * tiling.size.height as usize];
    for (row, y_sample) in output
        .chunks_exact_mut(tiling.size.width as usize)
        .zip(y_samples)
    {
        let Some((y0, y1, ty)) = y_sample else {
            continue;
        };
        for (pixel, x_sample) in row.iter_mut().zip(&x_samples) {
            let Some((x0, x1, tx)) = *x_sample else {
                continue;
            };
            let top = f32::from(source[y0 * width + x0]) * (1.0 - tx) +
                f32::from(source[y0 * width + x1]) * tx;
            let bottom = f32::from(source[y1 * width + x0]) * (1.0 - tx) +
                f32::from(source[y1 * width + x1]) * tx;
            *pixel = (top * (1.0 - ty) + bottom * ty).round() as u8;
        }
    }
    output
}

#[derive(malloc_size_of_derive::MallocSizeOf)]
pub(super) struct MaskPixels {
    #[conditional_malloc_size_of]
    pub bytes: Arc<Vec<u8>>,
    pub size: DeviceIntSize,
}

#[derive(Eq, Hash, PartialEq, malloc_size_of_derive::MallocSizeOf)]
struct LayerKey {
    image: Option<ImageKey>,
    #[ignore_malloc_size_of = "Inline geometry"]
    geometry: [u32; 20],
    composite: ImageMaskComposite,
}

#[derive(Eq, Hash, PartialEq, malloc_size_of_derive::MallocSizeOf)]
pub(super) struct CompositeKey {
    size: DeviceIntSize,
    layers: Vec<LayerKey>,
}

impl CompositeKey {
    pub(super) fn uses_any(&self, keys: &FxHashSet<ImageKey>) -> bool {
        self.layers
            .iter()
            .any(|layer| layer.image.is_some_and(|key| keys.contains(&key)))
    }
}

pub(super) fn composite_key(
    layers: &[ImageMaskLayer],
    size: DeviceIntSize,
) -> Option<CompositeKey> {
    if layers.is_empty() ||
        layers.len() > 32 ||
        size.width <= 0 ||
        size.height <= 0 ||
        size.width > 8192 ||
        size.height > 8192
    {
        return None;
    }
    let layers = layers
        .iter()
        .map(|layer| {
            if layer
                .rect
                .iter()
                .chain(layer.clip.iter())
                .chain(layer.rounded_clip.iter())
                .chain(layer.radii.iter().flatten())
                .any(|v| !v.is_finite())
            {
                return None;
            }
            if layer.image.is_some() &&
                (layer.rect[2] <= 0.0 ||
                    layer.rect[3] <= 0.0 ||
                    layer.clip[2] < 0.0 ||
                    layer.clip[3] < 0.0)
            {
                return None;
            }
            Some(LayerKey {
                image: layer.image,
                composite: layer.composite,
                geometry: [
                    layer.rect[0].to_bits(),
                    layer.rect[1].to_bits(),
                    layer.rect[2].to_bits(),
                    layer.rect[3].to_bits(),
                    layer.clip[0].to_bits(),
                    layer.clip[1].to_bits(),
                    layer.clip[2].to_bits(),
                    layer.clip[3].to_bits(),
                    layer.rounded_clip[0].to_bits(),
                    layer.rounded_clip[1].to_bits(),
                    layer.rounded_clip[2].to_bits(),
                    layer.rounded_clip[3].to_bits(),
                    layer.radii[0][0].to_bits(),
                    layer.radii[0][1].to_bits(),
                    layer.radii[1][0].to_bits(),
                    layer.radii[1][1].to_bits(),
                    layer.radii[2][0].to_bits(),
                    layer.radii[2][1].to_bits(),
                    layer.radii[3][0].to_bits(),
                    layer.radii[3][1].to_bits(),
                ],
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(CompositeKey { size, layers })
}

fn rounded_coverage(layer: &ImageMaskLayer, x: f32, y: f32) -> f32 {
    let [left, top, width, height] = layer.rounded_clip;
    let corners = [
        (left, top, 1.0, 1.0),
        (left + width, top, -1.0, 1.0),
        (left + width, top + height, -1.0, -1.0),
        (left, top + height, 1.0, -1.0),
    ];
    for ([rx, ry], (cx, cy, sx, sy)) in layer.radii.iter().copied().zip(corners) {
        if rx <= 0.0 || ry <= 0.0 {
            continue;
        }
        let dx = (x - cx) * sx;
        let dy = (y - cy) * sy;
        if dx >= rx + 0.5 || dy >= ry + 0.5 {
            continue;
        }
        let inside = |dx: f32, dy: f32| {
            let nx = ((rx - dx) / rx).max(0.0);
            let ny = ((ry - dy) / ry).max(0.0);
            nx * nx + ny * ny <= 1.0
        };
        if inside(dx - 0.5, dy - 0.5) {
            continue;
        }
        if !inside(dx + 0.5, dy + 0.5) {
            return 0.0;
        }
        let mut covered = 0;
        for iy in 0..4 {
            for ix in 0..4 {
                if inside(
                    dx - 0.5 + (ix as f32 + 0.5) / 4.0,
                    dy - 0.5 + (iy as f32 + 0.5) / 4.0,
                ) {
                    covered += 1;
                }
            }
        }
        return covered as f32 / 16.0;
    }
    1.0
}

struct MaskSampler<'a> {
    pixels: &'a MaskPixels,
    direct_origin: Option<(i32, i32)>,
    rounded: bool,
}

impl<'a> MaskSampler<'a> {
    fn new(pixels: &'a MaskPixels, layer: &ImageMaskLayer) -> Self {
        let [x, y, width, height] = layer.rect;
        let direct_origin = (width == pixels.size.width as f32 &&
            height == pixels.size.height as f32 &&
            x.fract() == 0.0 &&
            y.fract() == 0.0 &&
            x >= i32::MIN as f32 &&
            x < i32::MAX as f32 &&
            y >= i32::MIN as f32 &&
            y < i32::MAX as f32)
            .then_some((x as i32, y as i32));
        Self {
            pixels,
            direct_origin,
            rounded: layer.radii.iter().any(|r| r[0] > 0.0 && r[1] > 0.0),
        }
    }
}

fn layer_value(sampler: &MaskSampler<'_>, layer: &ImageMaskLayer, x: f32, y: f32) -> f32 {
    let pixels = sampler.pixels;
    let [cx, cy, cw, ch] = layer.clip;
    let [rx, ry, rw, rh] = layer.rect;
    if x < cx ||
        y < cy ||
        x >= cx + cw ||
        y >= cy + ch ||
        x < rx ||
        y < ry ||
        x >= rx + rw ||
        y >= ry + rh
    {
        return 0.0;
    }
    let coverage = if sampler.rounded {
        rounded_coverage(layer, x, y)
    } else {
        1.0
    };
    if coverage == 0.0 {
        return 0.0;
    }
    let width = pixels.size.width as usize;
    if let Some((ox, oy)) = sampler.direct_origin {
        let ix = (x as i64 - i64::from(ox)) as usize;
        let iy = (y as i64 - i64::from(oy)) as usize;
        return f32::from(pixels.bytes[iy * width + ix]) / 255.0 * coverage;
    }
    let height = pixels.size.height as usize;
    let tx = ((x - rx) / rw * width as f32 - 0.5).clamp(0.0, (width - 1) as f32);
    let ty = ((y - ry) / rh * height as f32 - 0.5).clamp(0.0, (height - 1) as f32);
    let x0 = tx.floor() as usize;
    let x1 = (x0 + 1).min(width - 1);
    let y0 = ty.floor() as usize;
    let y1 = (y0 + 1).min(height - 1);
    let a = f32::from(pixels.bytes[y0 * width + x0]) * (1.0 - tx.fract()) +
        f32::from(pixels.bytes[y0 * width + x1]) * tx.fract();
    let b = f32::from(pixels.bytes[y1 * width + x0]) * (1.0 - tx.fract()) +
        f32::from(pixels.bytes[y1 * width + x1]) * tx.fract();
    (a * (1.0 - ty.fract()) + b * ty.fract()) / 255.0 * coverage
}

pub(super) fn composite(
    layers: &[ImageMaskLayer],
    size: DeviceIntSize,
    images: &FxHashMap<ImageKey, MaskPixels>,
) -> Option<Vec<u8>> {
    let sources = layers
        .iter()
        .map(|layer| match layer.image {
            Some(key) => images
                .get(&key)
                .map(|pixels| Some(MaskSampler::new(pixels, layer))),
            None => Some(None),
        })
        .collect::<Option<Vec<_>>>()?;
    let mut output = vec![0; size.width as usize * size.height as usize];
    for y in 0..size.height {
        for x in 0..size.width {
            let mut destination = 0.0;
            for (index, (layer, source)) in layers.iter().zip(&sources).enumerate().rev() {
                let source = source.as_ref().map_or(0.0, |image| {
                    layer_value(image, layer, x as f32 + 0.5, y as f32 + 0.5)
                });
                destination = if index == layers.len() - 1 {
                    source
                } else {
                    match layer.composite {
                        ImageMaskComposite::Add => source + destination * (1.0 - source),
                        ImageMaskComposite::Subtract => source * (1.0 - destination),
                        ImageMaskComposite::Intersect => source * destination,
                        ImageMaskComposite::Exclude => {
                            source * (1.0 - destination) + destination * (1.0 - source)
                        },
                    }
                };
            }
            output[y as usize * size.width as usize + x as usize] =
                (destination * 255.0).round() as u8;
        }
    }
    Some(output)
}
