/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use net_traits::image_cache::ImageMaskTiling;

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
