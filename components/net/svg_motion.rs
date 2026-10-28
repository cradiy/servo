/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use kurbo::{BezPath, ParamCurve, ParamCurveArclen, ParamCurveDeriv, PathSeg};
use std::ops::Range;

struct Motion {
    insertion: usize,
    segments: Vec<(PathSeg, f64)>,
    length: f64,
    duration: f64,
    begin: f64,
    points: Vec<f64>,
    times: Vec<f64>,
    rotate: Option<f64>,
    reverse: bool,
    end: usize,
}

impl Motion {
    fn position(&self, seconds: f64) -> (kurbo::Point, f64) {
        if seconds < self.begin {
            return (kurbo::Point::ZERO, 0.0);
        }
        let progress = ((seconds - self.begin) / self.duration).rem_euclid(1.0);
        let index = self
            .times
            .partition_point(|time| *time <= progress)
            .saturating_sub(1)
            .min(self.times.len() - 2);
        let fraction = (progress - self.times[index]) / (self.times[index + 1] - self.times[index]);
        let distance = (self.points[index] +
            fraction * (self.points[index + 1] - self.points[index])) *
            self.length;
        let mut remaining = distance;
        let mut selected = self.segments.last().unwrap();
        for segment in &self.segments {
            selected = segment;
            if remaining <= segment.1 {
                break;
            }
            remaining -= segment.1;
        }
        let parameter = selected
            .0
            .inv_arclen(remaining.clamp(0.0, selected.1), 0.001);
        let point = selected.0.eval(parameter);
        let angle = self.rotate.unwrap_or_else(|| {
            let tangent = match selected.0 {
                PathSeg::Line(line) => line.deriv().eval(parameter),
                PathSeg::Quad(curve) => curve.deriv().eval(parameter),
                PathSeg::Cubic(curve) => curve.deriv().eval(parameter),
            };
            tangent.y.atan2(tangent.x).to_degrees() + if self.reverse { 180.0 } else { 0.0 }
        });

        (point, angle)
    }
}

pub(crate) struct PreparedMotion {
    base: resvg::tiny_skia::Pixmap,
    background: resvg::tiny_skia::Pixmap,
    foreground: resvg::tiny_skia::Pixmap,
    moving: resvg::usvg::Tree,
    scale: resvg::tiny_skia::Transform,
}

impl PreparedMotion {
    pub(crate) fn render(&self, document: &MotionDocument, seconds: f64) -> Vec<u8> {
        let (point, angle) = document.motions[0].position(seconds);
        let mut pixmap = self.base.clone();
        let transform = self
            .scale
            .pre_translate(point.x as f32, point.y as f32)
            .pre_rotate(angle as f32);
        let viewport =
            resvg::tiny_skia::IntRect::from_xywh(0, 0, pixmap.width(), pixmap.height()).unwrap();
        let dirty = self
            .moving
            .root()
            .abs_layer_bounding_box()
            .to_rect()
            .transform(transform)
            .and_then(|rect| rect.outset(2.0, 2.0))
            .and_then(|rect| rect.to_non_zero_rect())
            .map(|rect| rect.to_int_rect())
            .and_then(|rect| rect.intersect(&viewport));
        let Some(dirty) = dirty else {
            return pixmap.take();
        };
        let width = pixmap.width() as usize;
        let row_bytes = dirty.width() as usize * 4;
        for y in dirty.y()..dirty.bottom() {
            let start = (y as usize * width + dirty.x() as usize) * 4;
            pixmap.data_mut()[start..start + row_bytes]
                .copy_from_slice(&self.background.data()[start..start + row_bytes]);
        }
        resvg::render(&self.moving, transform, &mut pixmap.as_mut());
        if let Some(foreground) = self.foreground.clone_rect(dirty) {
            pixmap.draw_pixmap(
                dirty.x(),
                dirty.y(),
                foreground.as_ref(),
                &Default::default(),
                Default::default(),
                None,
            );
        }
        pixmap.take()
    }
}

pub(crate) struct MotionDocument {
    source: String,
    motions: Vec<Motion>,
    color_keyframes: bool,
}

fn clock(value: &str) -> Option<f64> {
    let value = value.trim();
    let seconds = if let Some(value) = value.strip_suffix("ms") {
        value.parse::<f64>().ok()? / 1000.0
    } else {
        value.strip_suffix('s').unwrap_or(value).parse().ok()?
    };
    seconds.is_finite().then_some(seconds)
}

fn numbers(value: &str) -> Option<Vec<f64>> {
    value
        .split(';')
        .map(|part| {
            let value: f64 = part.trim().parse().ok()?;
            (value.is_finite() && (0.0..=1.0).contains(&value)).then_some(value)
        })
        .collect()
}

impl MotionDocument {
    pub(crate) fn parse(source: String) -> Option<Self> {
        if source.len() > 1024 * 1024 {
            return None;
        }
        let document = roxmltree::Document::parse(&source).ok()?;
        let mut motions = Vec::new();
        for node in document
            .descendants()
            .filter(|node| node.has_tag_name(("http://www.w3.org/2000/svg", "animateMotion")))
        {
            // Event-based timing, finite repeats, and additive motion require a SMIL timeline.
            if node.attribute("repeatCount") != Some("indefinite") ||
                node.attribute("end").is_some() ||
                node.attribute("repeatDur").is_some() ||
                node.attribute("additive").is_some() ||
                node.attribute("accumulate").is_some() ||
                node.attribute("href").is_some() ||
                node.attribute(("http://www.w3.org/1999/xlink", "href"))
                    .is_some()
            {
                continue;
            }
            let Some(duration) = node
                .attribute("dur")
                .and_then(clock)
                .filter(|value| *value > 0.0)
            else {
                continue;
            };
            let Some(begin) = clock(node.attribute("begin").unwrap_or("0")) else {
                continue;
            };
            let Some(parent) = node.parent_element() else {
                continue;
            };
            if motions
                .iter()
                .any(|motion: &Motion| motion.end == parent.range().end)
            {
                continue;
            }
            if parent == document.root_element() {
                continue;
            }
            let path = node
                .children()
                .find(|child| child.has_tag_name(("http://www.w3.org/2000/svg", "mpath")))
                .and_then(|child| {
                    child
                        .attribute("href")
                        .or_else(|| child.attribute(("http://www.w3.org/1999/xlink", "href")))
                })
                .and_then(|href| href.strip_prefix('#'))
                .and_then(|id| {
                    document
                        .descendants()
                        .find(|candidate| candidate.attribute("id") == Some(id))
                })
                .and_then(|path| path.attribute("d"))
                .or_else(|| node.attribute("path"));
            let Some(path) = path.and_then(|path| BezPath::from_svg(path).ok()) else {
                continue;
            };
            let segments: Vec<_> = path
                .segments()
                .map(|segment| (segment, segment.arclen(0.001)))
                .collect();
            if segments.len() > 4096 {
                continue;
            }
            let length: f64 = segments.iter().map(|(_, length)| length).sum();
            if !length.is_finite() || length <= 0.0 {
                continue;
            }
            let mode = node.attribute("calcMode").unwrap_or("paced");
            if !matches!(mode, "linear" | "paced") {
                continue;
            }
            let (points, times) = if mode == "linear" && node.attribute("keyPoints").is_some() {
                let Some(points) = node.attribute("keyPoints").and_then(numbers) else {
                    continue;
                };
                let Some(times) = node.attribute("keyTimes").and_then(numbers) else {
                    continue;
                };
                if points.len() != times.len() ||
                    times.len() < 2 ||
                    times.len() > 4096 ||
                    times[0] != 0.0 ||
                    times.last() != Some(&1.0) ||
                    times.windows(2).any(|pair| pair[0] >= pair[1])
                {
                    continue;
                }
                (points, times)
            } else if mode == "linear" {
                let mut points = vec![0.0];
                let mut distance = 0.0;
                for (_, segment_length) in &segments {
                    distance += segment_length;
                    points.push(distance / length);
                }
                let times = if let Some(value) = node.attribute("keyTimes") {
                    let Some(times) = numbers(value) else {
                        continue;
                    };
                    if times.len() != points.len() ||
                        times.first() != Some(&0.0) ||
                        times.last() != Some(&1.0) ||
                        times.windows(2).any(|pair| pair[0] >= pair[1])
                    {
                        continue;
                    }
                    times
                } else {
                    (0..points.len())
                        .map(|index| index as f64 / segments.len() as f64)
                        .collect()
                };
                (points, times)
            } else {
                (vec![0.0, 1.0], vec![0.0, 1.0])
            };
            let rotation = node.attribute("rotate").unwrap_or("0");
            let rotate = match rotation {
                "auto" | "auto-reverse" => None,
                value => match value.parse::<f64>() {
                    Ok(angle) if angle.is_finite() => Some(angle),
                    _ => continue,
                },
            };
            let range = parent.range();
            // A wrapper applies the motion after the target's own transform.
            motions.push(Motion {
                insertion: range.start,
                end: range.end,
                segments,
                length,
                duration,
                begin,
                points,
                times,
                rotate,
                reverse: rotation == "auto-reverse",
            });
            if motions.len() > 64 {
                return None;
            }
        }
        let color_keyframes = crate::svg_css::has_color_keyframes(&source);
        if motions.is_empty() && !color_keyframes {
            return None;
        }
        drop(document);
        Some(Self {
            source,
            motions,
            color_keyframes,
        })
    }

    pub(crate) fn prepare(
        &self,
        options: &resvg::usvg::Options,
        size: resvg::tiny_skia::IntSize,
        natural_size: resvg::tiny_skia::IntSize,
    ) -> Option<PreparedMotion> {
        if self.color_keyframes
            || self.motions.len() != 1
            || size.width() as usize * size.height() as usize * 12 > 32 * 1024 * 1024
        {
            return None;
        }
        let document = roxmltree::Document::parse(&self.source).ok()?;
        let root = document.root_element();
        // Layer separation is only valid without root-level compositing effects.
        if ["transform", "filter", "mask", "clip-path", "opacity"]
            .iter()
            .any(|name| root.attribute(*name).is_some()) ||
            root.attribute("style").is_some_and(|style| {
                [
                    "opacity",
                    "filter",
                    "mask",
                    "clip-path",
                    "blend",
                    "transform",
                ]
                .iter()
                .any(|name| style.contains(name))
            })
        {
            return None;
        }
        if let Some(viewbox) = root.attribute("viewBox") {
            let values: Vec<f64> = viewbox
                .split(|c: char| c.is_ascii_whitespace() || c == ',')
                .filter(|s| !s.is_empty())
                .map(str::parse)
                .collect::<Result<_, _>>()
                .ok()?;
            if values !=
                [
                    0.0,
                    0.0,
                    natural_size.width() as f64,
                    natural_size.height() as f64,
                ]
            {
                return None;
            }
        }
        let motion = &self.motions[0];
        let children: Vec<_> = root
            .children()
            .filter(|node| {
                node.is_element() &&
                    !matches!(
                        node.tag_name().name(),
                        "defs" | "style" | "title" | "desc" | "metadata"
                    )
            })
            .collect();
        // Keep referenced definitions and backdrop-dependent compositing on the full-tree path.
        if children.iter().any(|child| {
            child
                .descendants()
                .filter(|node| node.is_element())
                .any(|node| {
                    node.attribute("id").is_some() ||
                        node.has_tag_name(("http://www.w3.org/2000/svg", "defs")) ||
                        node.attribute("style")
                            .is_some_and(|style| style.contains("mix-blend-mode"))
                })
        }) {
            return None;
        }
        let target = children
            .iter()
            .position(|node| node.range().start == motion.insertion)?;
        let mut sources = Vec::new();
        for layer in 0..3 {
            let mut source = self.source.clone();
            for (index, child) in children.iter().enumerate().rev() {
                let keep = match layer {
                    0 => index < target,
                    1 => index == target,
                    _ => index > target,
                };
                if !keep {
                    source.replace_range(child.range(), "");
                }
            }
            sources.push(source);
        }
        let scale = resvg::tiny_skia::Transform::from_scale(
            size.width() as f32 / natural_size.width() as f32,
            size.height() as f32 / natural_size.height() as f32,
        );
        let raster = |source: &str| {
            let tree = resvg::usvg::Tree::from_str(source, options).ok()?;
            let mut pixels = resvg::tiny_skia::Pixmap::new(size.width(), size.height())?;
            resvg::render(&tree, scale, &mut pixels.as_mut());
            Some(pixels)
        };
        let background = raster(&sources[0])?;
        let foreground = raster(&sources[2])?;
        let mut base = background.clone();
        base.draw_pixmap(
            0,
            0,
            foreground.as_ref(),
            &Default::default(),
            Default::default(),
            None,
        );
        Some(PreparedMotion {
            base,
            background,
            foreground,
            moving: resvg::usvg::Tree::from_str(&sources[1], options).ok()?,
            scale,
        })
    }

    pub(crate) fn sample(&self, seconds: f64) -> String {
        let mut edits: Vec<(Range<usize>, String)> = Vec::new();
        for motion in &self.motions {
            if seconds < motion.begin {
                continue;
            }
            let (point, angle) = motion.position(seconds);
            edits.push((motion.insertion..motion.insertion, format!("<g xmlns=\"http://www.w3.org/2000/svg\" transform=\"translate({} {}) rotate({})\">", point.x, point.y, angle)));
            edits.push((motion.end..motion.end, "</g>".to_owned()));
        }
        edits.sort_by_key(|(range, _)| std::cmp::Reverse(range.start));
        let mut result = self.source.clone();
        for (range, replacement) in edits {
            result.replace_range(range, &replacement);
        }
        if self.color_keyframes {
            crate::svg_css::sample_animations(&result, seconds).unwrap_or(result)
        } else {
            result
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn motion_samples_timing_path_and_loop() {
        let source = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="100"><circle r="2"><animateMotion path="M 10 20 L 30 20 L 30 60" dur="4s" repeatCount="indefinite" calcMode="linear" keyTimes="0;0.5;1" keyPoints="0;0.3333333333333333;1"/></circle></svg>"#;
        let motion = MotionDocument::parse(source.to_owned()).unwrap();
        assert!(motion.sample(0.0).contains("translate(10 20)"));
        assert!(motion.sample(2.0).contains("translate(30 20)"));
        let frame = motion.sample(3.0);
        let tree = resvg::usvg::Tree::from_str(&frame, &Default::default()).unwrap();
        let mut pixmap = resvg::tiny_skia::Pixmap::new(100, 100).unwrap();
        resvg::render(&tree, Default::default(), &mut pixmap.as_mut());
        assert_eq!(pixmap.pixel(30, 40).unwrap().alpha(), 255);
        assert_eq!(pixmap.pixel(10, 20).unwrap().alpha(), 0);
        assert_eq!(motion.sample(0.0), motion.sample(4.0));
    }

    #[test]
    fn cached_layers_preserve_occlusion_and_rotated_motion() {
        let source = r#"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="80" viewBox="0 0 80 80"><rect width="80" height="80" fill="navy"/><g><rect x="-4" y="-2" width="8" height="4" fill="orange"/><animateMotion path="M 12 18 L 56 62" rotate="auto" dur="3s" repeatCount="indefinite"/></g><rect x="30" y="30" width="20" height="20" fill="green"/></svg>"#;
        let motion = MotionDocument::parse(source.to_owned()).unwrap();
        let size = resvg::tiny_skia::IntSize::from_wh(160, 160).unwrap();
        let natural = resvg::tiny_skia::IntSize::from_wh(80, 80).unwrap();
        let options = resvg::usvg::Options::default();
        let prepared = motion.prepare(&options, size, natural).unwrap();
        for seconds in [0.0, 0.25, 1.5, 2.5, 3.0] {
            let tree = resvg::usvg::Tree::from_str(&motion.sample(seconds), &options).unwrap();
            let mut expected = resvg::tiny_skia::Pixmap::new(160, 160).unwrap();
            resvg::render(
                &tree,
                resvg::tiny_skia::Transform::from_scale(2.0, 2.0),
                &mut expected.as_mut(),
            );
            let actual = prepared.render(&motion, seconds);
            assert!(
                actual
                    .iter()
                    .zip(expected.data())
                    .all(|(a, b)| a.abs_diff(*b) <= 1),
                "layer mismatch at {seconds}"
            );
        }
    }
}
