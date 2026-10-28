/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use super::{Declaration, declarations, expand};
use cssparser::{Parser, Token};

pub(super) struct Keyframes {
    pub name: String,
    frames: Vec<(f64, Vec<Declaration>)>,
}

struct Sheet;
impl<'i> cssparser::AtRuleParser<'i> for Sheet {
    type Prelude = String;
    type AtRule = Keyframes;
    type Error = ();
    fn parse_prelude(
        &mut self,
        name: cssparser::CowRcStr<'i>,
        input: &mut Parser<'i>,
    ) -> Result<String, cssparser::ParseError<()>> {
        if !name.eq_ignore_ascii_case("keyframes") {
            return Err(cssparser::ParseError::custom(()));
        }
        Ok(input.expect_ident_or_string()?.to_string())
    }
    fn parse_block(
        &mut self,
        name: String,
        _: &cssparser::ParserState,
        input: &mut Parser<'i>,
    ) -> Result<Keyframes, cssparser::ParseError<()>> {
        let mut frames = Vec::new();
        for (offsets, values) in
            cssparser::RuleBodyParser::new(input, &mut Frames).filter_map(Result::ok)
        {
            for offset in offsets {
                frames.push((offset, values.clone()));
            }
        }
        frames.sort_by(|a, b| a.0.total_cmp(&b.0));
        Ok(Keyframes { name, frames })
    }
}
impl<'i> cssparser::QualifiedRuleParser<'i> for Sheet {
    type Prelude = ();
    type QualifiedRule = Keyframes;
    type Error = ();
}
struct Frames;
impl<'i> cssparser::QualifiedRuleParser<'i> for Frames {
    type Prelude = Vec<f64>;
    type QualifiedRule = (Vec<f64>, Vec<Declaration>);
    type Error = ();
    fn parse_prelude(
        &mut self,
        input: &mut Parser<'i>,
    ) -> Result<Vec<f64>, cssparser::ParseError<()>> {
        input.parse_comma_separated(|p| match p.next()?.clone() {
            Token::Percentage { unit_value, .. } if (0.0..=1.0).contains(&unit_value) => {
                Ok(unit_value as f64)
            },
            Token::Ident(s) if s.eq_ignore_ascii_case("from") => Ok(0.0),
            Token::Ident(s) if s.eq_ignore_ascii_case("to") => Ok(1.0),
            _ => Err(cssparser::ParseError::custom(())),
        })
    }
    fn parse_block(
        &mut self,
        offsets: Vec<f64>,
        _: &cssparser::ParserState,
        input: &mut Parser<'i>,
    ) -> Result<Self::QualifiedRule, cssparser::ParseError<()>> {
        Ok((offsets, declarations(input)))
    }
}
impl<'i> cssparser::AtRuleParser<'i> for Frames {
    type Prelude = ();
    type AtRule = (Vec<f64>, Vec<Declaration>);
    type Error = ();
}
impl<'i> cssparser::DeclarationParser<'i> for Frames {
    type Declaration = (Vec<f64>, Vec<Declaration>);
    type Error = ();
}
impl<'i> cssparser::RuleBodyItemParser<'i, (Vec<f64>, Vec<Declaration>), ()> for Frames {
    fn parse_declarations(&self) -> bool {
        false
    }
    fn parse_qualified(&self) -> bool {
        true
    }
}

pub(super) fn parse(css: &str) -> Vec<Keyframes> {
    cssparser::StyleSheetParser::new(&mut Parser::new(css), &mut Sheet)
        .filter_map(Result::ok)
        .filter(|keyframes| {
            keyframes.frames.iter().any(|(_, declarations)| {
                declarations.iter().any(|declaration| {
                    !declaration.important
                        && matches!(declaration.name.as_str(), "fill" | "stroke")
                })
            })
        })
        .collect()
}

fn clock(s: &str) -> Option<f64> {
    let s = s.trim();
    let value = if let Some(s) = s.strip_suffix("ms") {
        s.parse::<f64>().ok()? / 1000.0
    } else {
        s.strip_suffix('s')?.parse::<f64>().ok()?
    };
    value.is_finite().then_some(value)
}

fn mix(a: &str, b: &str, t: f64) -> Option<String> {
    let a: svgtypes::Color = a.parse().ok()?;
    let b: svgtypes::Color = b.parse().ok()?;
    let aa = a.alpha as f64 / 255.0;
    let ba = b.alpha as f64 / 255.0;
    let alpha = aa * (1.0 - t) + ba * t;
    let channel = |a: u8, b: u8| {
        if alpha == 0.0 {
            0.0
        } else {
            (a as f64 * aa * (1.0 - t) + b as f64 * ba * t) / alpha
        }
    };
    Some(format!(
        "rgba({},{},{},{alpha})",
        channel(a.red, b.red).round(),
        channel(a.green, b.green).round(),
        channel(a.blue, b.blue).round()
    ))
}

pub(super) fn sample(
    frames: &[Keyframes],
    seconds: f64,
    mut property: impl FnMut(&str) -> Option<String>,
    mut variable: impl FnMut(&str) -> Option<String>,
) -> Vec<(String, String)> {
    let shorthand = property("animation").unwrap_or_default();
    let mut name = None;
    let mut duration = None;
    let mut delay = 0.0;
    let mut infinite = false;
    let mut linear = false;
    for token in shorthand.split_whitespace() {
        if let Some(time) = clock(token) {
            if duration.is_none() {
                duration = Some(time)
            } else {
                delay = time
            }
        } else if token == "linear" {
            linear = true
        } else if token == "infinite" {
            infinite = true
        } else if frames.iter().any(|frame| frame.name == token) {
            name = Some(token.to_owned())
        } else if token != "normal" && token != "running" && token != "none" {
            return Vec::new();
        }
    }
    if let Some(value) = property("animation-name") {
        name = Some(value)
    }
    if let Some(value) = property("animation-duration") {
        duration = clock(&value)
    }
    if let Some(value) = property("animation-delay") {
        let Some(value) = clock(&value) else {
            return Vec::new();
        };
        delay = value;
    }
    if let Some(value) = property("animation-timing-function") {
        linear = value == "linear"
    }
    if let Some(value) = property("animation-iteration-count") {
        infinite = value == "infinite"
    }
    if property("animation-direction").is_some_and(|v| v != "normal")
        || property("animation-play-state").is_some_and(|v| v != "running")
    {
        return Vec::new();
    }
    let Some(duration) = duration.filter(|duration| *duration > 0.0) else {
        return Vec::new();
    };
    if !linear || !infinite || seconds < delay {
        return Vec::new();
    }
    let Some(keyframes) = frames
        .iter()
        .rev()
        .find(|frame| Some(&frame.name) == name.as_ref())
    else {
        return Vec::new();
    };
    let progress = ((seconds - delay) / duration).rem_euclid(1.0);
    let mut sampled = Vec::new();
    for prop in ["fill", "stroke"] {
        let mut track: Vec<(f64, String)> = Vec::new();
        for (offset, declarations) in &keyframes.frames {
            if let Some(declaration) = declarations
                .iter()
                .rev()
                .find(|d| d.name == prop && !d.important)
            {
                let Some(value) = expand(&declaration.value, &mut variable, 0) else {
                    continue;
                };
                if track.last().is_some_and(|last| last.0 == *offset) {
                    track.pop();
                }
                track.push((*offset, value));
            }
        }
        if track.first().is_none_or(|x| x.0 != 0.0) || track.last().is_none_or(|x| x.0 != 1.0) {
            continue;
        }
        let index = track
            .partition_point(|(offset, _)| *offset <= progress)
            .saturating_sub(1)
            .min(track.len() - 2);
        let (left, right) = (&track[index], &track[index + 1]);
        let t = (progress - left.0) / (right.0 - left.0);
        if let Some(value) = mix(&left.1, &right.1, t) {
            sampled.push((prop.to_owned(), value));
        }
    }
    sampled
}

#[cfg(test)]
mod paint_animation_tests {
    fn pixel(source: &str, seconds: f64, x: usize) -> Vec<u8> {
        let source = crate::svg_css::sample_animations(source, seconds).unwrap();
        let tree = resvg::usvg::Tree::from_str(&source, &Default::default()).unwrap();
        let mut image = resvg::tiny_skia::Pixmap::new(24, 8).unwrap();
        resvg::render(&tree, Default::default(), &mut image.as_mut());
        image.data()[(4 * 24 + x) * 4..(4 * 24 + x) * 4 + 4].to_vec()
    }

    #[test]
    fn svg_color_keyframes_resolve_variables_delays_and_important_styles() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="8">
          <style>:root {--start:#800000;--end:#000080}
            rect {fill:#008000;animation:paint-cycle 4s linear infinite}
            @keyframes paint-cycle {from {fill:var(--start)} to {fill:var(--end)}}
          </style>
          <rect width="8" height="8"/>
          <rect x="8" width="8" height="8" style="animation-delay:1s"/>
          <rect x="16" width="8" height="8" style="fill:#008000!important"/>
        </svg>"##;
        assert_eq!(pixel(svg, 0.0, 4), [128, 0, 0, 255]);
        assert_eq!(pixel(svg, 2.0, 4), [64, 0, 64, 255]);
        assert_eq!(pixel(svg, 4.0, 4), [128, 0, 0, 255]);
        assert_eq!(pixel(svg, 0.5, 12), [0, 128, 0, 255]);
        assert_eq!(pixel(svg, 3.0, 12), [64, 0, 64, 255]);
        assert_eq!(pixel(svg, 2.0, 20), [0, 128, 0, 255]);
    }

    #[test]
    fn svg_color_keyframes_merge_duplicate_offsets_and_premultiply_alpha() {
        let svg = r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="8">
          <style>rect {animation:fade-paint 2s linear infinite}
            @keyframes fade-paint {
              0% {fill:blue} 0% {fill:rgba(255,0,0,0)}
              100% {fill:rgb(0,128,0)}
            }
          </style><rect width="24" height="8"/>
        </svg>"##;
        assert_eq!(pixel(svg, 1.0, 4), [0, 64, 0, 128]);
    }
}
