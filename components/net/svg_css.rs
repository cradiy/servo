/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::collections::{HashMap, HashSet};

use cssparser::{Parser, ToCss, Token};

const MAX_VALUE: usize = 1024 * 1024;

fn expand(
    value: &str,
    lookup: &mut impl FnMut(&str) -> Option<String>,
    depth: usize,
) -> Option<String> {
    if depth > 64 {
        return None;
    }
    fn walk<'i>(
        p: &mut Parser<'i>,
        lookup: &mut impl FnMut(&str) -> Option<String>,
        depth: usize,
    ) -> Result<String, cssparser::ParseError<()>> {
        let mut out = String::new();
        while let Ok(token) = p.next_including_whitespace_and_comments().cloned() {
            match token {
                Token::Function(ref name) if name.eq_ignore_ascii_case("var") => {
                    let replacement = p.parse_nested_block(|p| {
                        let name = p.expect_ident_cloned()?;
                        if !name.starts_with("--") {
                            return Err(cssparser::ParseError::custom(()));
                        }
                        let value = lookup(&name);
                        let fallback = if p.try_parse(|p| p.expect_comma()).is_ok() {
                            let start = p.position();
                            while p.next_including_whitespace_and_comments().is_ok() {}
                            Some(p.slice_from(start).to_owned())
                        } else {
                            None
                        };
                        value
                            .or_else(|| fallback.and_then(|v| expand(&v, lookup, depth + 1)))
                            .ok_or_else(|| cssparser::ParseError::custom(()))
                    })?;
                    out.push_str(&replacement);
                },
                Token::Function(_) |
                Token::ParenthesisBlock |
                Token::SquareBracketBlock |
                Token::CurlyBracketBlock => {
                    if depth >= 64 {
                        return Err(cssparser::ParseError::custom(()));
                    }
                    token.to_css(&mut out).unwrap();
                    let end = match token {
                        Token::SquareBracketBlock => ']',
                        Token::CurlyBracketBlock => '}',
                        _ => ')',
                    };
                    out.push_str(&p.parse_nested_block(|p| walk(p, lookup, depth + 1))?);
                    out.push(end);
                },
                _ => token.to_css(&mut out).unwrap(),
            }
            if out.len() > MAX_VALUE {
                return Err(cssparser::ParseError::custom(()));
            }
        }
        Ok(out)
    }
    walk(&mut Parser::new(value), lookup, depth).ok()
}

fn dependencies(value: &str) -> Vec<String> {
    fn visit(p: &mut Parser<'_>, found: &mut Vec<String>) -> Result<(), cssparser::ParseError<()>> {
        while let Ok(token) = p.next().cloned() {
            match token {
                Token::Function(name) => p.parse_nested_block(|p| {
                    if name.eq_ignore_ascii_case("var") {
                        if let Ok(name) = p.expect_ident_cloned() {
                            found.push(name.to_string());
                        }
                    }
                    visit(p, found)
                })?,
                Token::ParenthesisBlock | Token::SquareBracketBlock | Token::CurlyBracketBlock => {
                    p.parse_nested_block(|p| visit(p, found))?;
                },
                _ => (),
            }
        }
        Ok(())
    }
    let mut found = Vec::new();
    let _ = visit(&mut Parser::new(value), &mut found);
    found
}

fn cyclic(
    name: &str,
    graph: &HashMap<String, Vec<String>>,
    path: &mut Vec<String>,
    invalid: &mut HashSet<String>,
    done: &mut HashSet<String>,
) {
    if let Some(index) = path.iter().position(|v| v == name) {
        invalid.extend(path[index..].iter().cloned());
        return;
    }
    if done.contains(name) {
        return;
    }
    if path.len() >= 64 {
        invalid.insert(name.to_owned());
        return;
    }
    if let Some(edges) = graph.get(name) {
        path.push(name.to_owned());
        for edge in edges {
            cyclic(edge, graph, path, invalid, done);
        }
        path.pop();
    }
    done.insert(name.to_owned());
}

fn resolve(
    name: &str,
    raw: &HashMap<String, String>,
    inherited: &HashMap<String, String>,
    visiting: &mut Vec<String>,
    invalid: &mut HashSet<String>,
    depth: usize,
    memo: &mut HashMap<String, Option<String>>,
) -> Option<String> {
    if let Some(value) = memo.get(name) {
        return value.clone();
    }
    if depth > 64 || invalid.contains(name) {
        return None;
    }
    if let Some(index) = visiting.iter().position(|v| v == name) {
        invalid.extend(visiting[index..].iter().cloned());
        return None;
    }
    let Some(value) = raw.get(name) else {
        return inherited.get(name).cloned();
    };
    match value.trim() {
        "inherit" | "unset" => return inherited.get(name).cloned(),
        "initial" | "revert" | "revert-layer" => return None,
        _ => (),
    }
    visiting.push(name.to_owned());
    let result = expand(
        value,
        &mut |dependency| {
            resolve(
                dependency,
                raw,
                inherited,
                visiting,
                invalid,
                depth + 1,
                memo,
            )
        },
        depth,
    );
    visiting.pop();
    let result = if invalid.contains(name) { None } else { result };
    if memo
        .values()
        .filter_map(|v| v.as_ref())
        .map(String::len)
        .sum::<usize>() +
        result.as_ref().map_or(0, String::len) >
        16 * MAX_VALUE
    {
        return None;
    }
    memo.insert(name.to_owned(), result.clone());
    result
}

struct Declaration {
    name: String,
    value: String,
    important: bool,
}
struct Declarations;
impl<'i> cssparser::DeclarationParser<'i> for Declarations {
    type Declaration = Declaration;
    type Error = ();
    fn parse_value(
        &mut self,
        name: cssparser::CowRcStr<'i>,
        input: &mut Parser<'i>,
        _: &cssparser::ParserState,
    ) -> Result<Declaration, cssparser::ParseError<()>> {
        let start = input.position();
        let mut end = start;
        let mut important = false;
        while !input.is_exhausted() {
            let state = input.state();
            if input.try_parse(cssparser::parse_important).is_ok() && input.is_exhausted() {
                important = true;
                break;
            }
            input.reset(&state);
            let token = input.next_including_whitespace_and_comments()?.clone();
            if matches!(
                token,
                Token::Function(_) |
                    Token::ParenthesisBlock |
                    Token::SquareBracketBlock |
                    Token::CurlyBracketBlock
            ) {
                input.parse_nested_block(|p| {
                    while p.next_including_whitespace_and_comments().is_ok() {}
                    Ok::<_, cssparser::ParseError<()>>(())
                })?;
            }
            end = input.position();
        }
        Ok(Declaration {
            name: if name.starts_with("--") {
                name.to_string()
            } else {
                name.to_ascii_lowercase()
            },
            value: input.slice(start..end).trim().to_owned(),
            important,
        })
    }
}
impl<'i> cssparser::AtRuleParser<'i> for Declarations {
    type Prelude = ();
    type AtRule = Declaration;
    type Error = ();
}
impl<'i> cssparser::QualifiedRuleParser<'i> for Declarations {
    type Prelude = ();
    type QualifiedRule = Declaration;
    type Error = ();
}
impl<'i> cssparser::RuleBodyItemParser<'i, Declaration, ()> for Declarations {
    fn parse_declarations(&self) -> bool {
        true
    }
    fn parse_qualified(&self) -> bool {
        false
    }
}
fn declarations(input: &mut Parser<'_>) -> Vec<Declaration> {
    cssparser::RuleBodyParser::new(input, &mut Declarations)
        .filter_map(Result::ok)
        .collect()
}
struct Rule<'i> {
    selectors: Vec<simplecss::Selector<'i>>,
    declarations: Vec<Declaration>,
}
struct Rules;
impl<'i> cssparser::AtRuleParser<'i> for Rules {
    type Prelude = ();
    type AtRule = Rule<'i>;
    type Error = ();
}
impl<'i> cssparser::QualifiedRuleParser<'i> for Rules {
    type Prelude = Vec<simplecss::Selector<'i>>;
    type QualifiedRule = Rule<'i>;
    type Error = ();
    fn parse_prelude(
        &mut self,
        input: &mut Parser<'i>,
    ) -> Result<Self::Prelude, cssparser::ParseError<()>> {
        input.parse_comma_separated(|input| {
            let start = input.position();
            while input.next_including_whitespace_and_comments().is_ok() {}
            simplecss::Selector::parse(input.slice_from(start))
                .ok_or_else(|| cssparser::ParseError::custom(()))
        })
    }
    fn parse_block(
        &mut self,
        selectors: Self::Prelude,
        _: &cssparser::ParserState,
        input: &mut Parser<'i>,
    ) -> Result<Rule<'i>, cssparser::ParseError<()>> {
        Ok(Rule {
            selectors,
            declarations: declarations(input),
        })
    }
}

struct Element<'a, 'i>(roxmltree::Node<'a, 'i>, &'a str);
impl simplecss::Element for Element<'_, '_> {
    fn parent_element(&self) -> Option<Self> {
        self.0.parent_element().map(|n| Self(n, self.1))
    }
    fn prev_sibling_element(&self) -> Option<Self> {
        self.0.prev_sibling_element().map(|n| Self(n, self.1))
    }
    fn has_local_name(&self, name: &str) -> bool {
        self.0.tag_name().name() == name
    }
    fn attribute_matches(&self, name: &str, op: simplecss::AttributeOperator) -> bool {
        if name == self.1 {
            return self.0.parent_element().is_none() && op.matches("");
        }
        self.0.attribute(name).is_some_and(|v| op.matches(v))
    }
    fn pseudo_class_matches(&self, class: simplecss::PseudoClass) -> bool {
        matches!(class, simplecss::PseudoClass::FirstChild) &&
            self.0.prev_sibling_element().is_none()
    }
}

fn root_selector(css: &str, marker: &str) -> String {
    let mut parser = Parser::new(css);
    let mut out = String::new();
    let mut last = 0;
    while !parser.is_exhausted() {
        let start = parser.position();
        let Ok(token) = parser.next_including_whitespace_and_comments().cloned() else {
            break;
        };
        if token == Token::Colon &&
            parser
                .try_parse(|p| p.expect_ident_matching("root"))
                .is_ok()
        {
            out.push_str(&css[last..start.byte_index()]);
            out.push('[');
            out.push_str(marker);
            out.push(']');
            last = parser.position().byte_index();
        }
    }
    out.push_str(&css[last..]);
    out
}

fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
}

pub(crate) fn resolve_variables(text: &str) -> Option<String> {
    if !text.contains("--") && !text.to_ascii_lowercase().contains("var(") {
        return None;
    }
    let document = roxmltree::Document::parse(text).ok()?;
    let mut marker = "data-servo-svg-root".to_owned();
    while text.contains(&marker) {
        marker.push('x');
    }
    let css = document
        .descendants()
        .filter(|n| n.has_tag_name("style"))
        .filter(|n| n.attribute("type").is_none_or(|t| t == "text/css"))
        .filter_map(|n| n.text())
        .map(|s| root_selector(s, &marker))
        .collect::<Vec<_>>()
        .join("\n");
    let sheet = cssparser::StyleSheetParser::new(&mut Parser::new(&css), &mut Rules)
        .filter_map(Result::ok)
        .collect::<Vec<_>>();
    let mut computed: HashMap<roxmltree::NodeId, HashMap<String, String>> = HashMap::new();
    let mut edits = Vec::new();
    let mut size = text.len();
    let mut variable_bytes = 0usize;
    for node in document.descendants().filter(|n| n.is_element()) {
        let mut winners = HashMap::new();
        let mut put = |name: &str, value: &str, rank: (bool, bool, [u8; 3], usize)| {
            let slot = winners
                .entry(name.to_owned())
                .or_insert((String::new(), rank));
            if rank >= slot.1 {
                *slot = (value.to_owned(), rank);
            }
        };
        for attr in node
            .attributes()
            .filter(|a| a.name() != "style" && a.value().contains("var("))
        {
            put(attr.name(), attr.value(), (false, false, [0; 3], 0));
        }
        let mut order = 0;
        for rule in &sheet {
            if let Some(specificity) = rule
                .selectors
                .iter()
                .filter(|s| s.matches(&Element(node, &marker)))
                .map(|s| s.specificity())
                .max()
            {
                for declaration in &rule.declarations {
                    order += 1;
                    put(
                        &declaration.name,
                        &declaration.value,
                        (declaration.important, false, specificity, order),
                    );
                }
            }
        }
        for declaration in declarations(&mut Parser::new(node.attribute("style").unwrap_or(""))) {
            order += 1;
            put(
                &declaration.name,
                &declaration.value,
                (declaration.important, true, [0; 3], order),
            );
        }
        let inherited = node
            .parent_element()
            .and_then(|n| computed.get(&n.id()))
            .cloned()
            .unwrap_or_default();
        let raw = winners
            .iter()
            .filter(|(name, _)| name.starts_with("--"))
            .map(|(name, (value, _))| (name.clone(), value.clone()))
            .collect::<HashMap<_, _>>();
        if raw.len() > 4096 {
            return None;
        }
        let mut invalid = HashSet::new();
        let graph = raw
            .iter()
            .map(|(name, value)| (name.clone(), dependencies(value)))
            .collect::<HashMap<_, _>>();
        let mut done = HashSet::new();
        for name in raw.keys() {
            cyclic(name, &graph, &mut Vec::new(), &mut invalid, &mut done);
        }
        let mut memo = HashMap::new();
        for name in raw.keys() {
            let _ = resolve(
                name,
                &raw,
                &inherited,
                &mut Vec::new(),
                &mut invalid,
                0,
                &mut memo,
            );
        }
        let mut variables = inherited.clone();
        for name in raw.keys() {
            variables.remove(name);
            if let Some(value) = resolve(
                name,
                &raw,
                &inherited,
                &mut Vec::new(),
                &mut invalid,
                0,
                &mut memo,
            ) {
                variables.insert(name.clone(), value);
            }
        }
        let mut style = String::new();
        let mut properties = winners
            .iter()
            .filter(|(name, _)| !name.starts_with("--"))
            .collect::<Vec<_>>();
        properties.sort_by_key(|(_, (_, rank))| *rank);
        for (name, (value, _)) in properties {
            let value = expand(value, &mut |name| variables.get(name).cloned(), 0)
                .unwrap_or_else(|| "inherit".to_owned());
            if !style.is_empty() {
                style.push(';');
            }
            style.push_str(name);
            style.push(':');
            style.push_str(&value);
            style.push_str(" !important");
        }
        variable_bytes =
            variable_bytes.checked_add(variables.iter().map(|(k, v)| k.len() + v.len()).sum())?;
        if variable_bytes > 16 * MAX_VALUE {
            return None;
        }
        computed.insert(node.id(), variables);
        if style != node.attribute("style").unwrap_or("") {
            let (range, replacement) = if let Some(attr) =
                node.attributes().find(|a| a.name() == "style")
            {
                (attr.range(), format!("style=\"{}\"", escape(&style)))
            } else {
                let start = node.range().start + 1;
                let end = start +
                    text[start..].find(|c: char| c.is_whitespace() || c == '>' || c == '/')?;
                (end..end, format!(" style=\"{}\"", escape(&style)))
            };
            size = size.checked_add(replacement.len())?;
            if size > text.len().saturating_add(16 * MAX_VALUE) {
                return None;
            }
            edits.push((range, replacement));
        }
    }
    let mut result = text.to_owned();
    for (range, replacement) in edits.into_iter().rev() {
        result.replace_range(range, &replacement);
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(source: &str) -> Vec<u8> {
        let resolved = resolve_variables(source).unwrap();
        let tree =
            resvg::usvg::Tree::from_str(&resolved, &resvg::usvg::Options::default()).unwrap();
        let mut pixmap = resvg::tiny_skia::Pixmap::new(40, 8).unwrap();
        resvg::render(
            &tree,
            resvg::tiny_skia::Transform::identity(),
            &mut pixmap.as_mut(),
        );
        pixmap.data().to_vec()
    }

    #[test]
    fn svg_variables_inherit_computed_values_and_respect_cascade() {
        let pixels = render(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="8">
          <style>:root { --ink:#a02040; --alias:var(--ink); }
            rect {fill:var(--ink)} .scope {--ink:#2040a0!important}
          </style>
          <rect width="8" height="8"/>
          <g class="scope" style="--ink:#20a040">
            <rect x="8" width="8" height="8"/>
            <rect x="16" width="8" height="8" style="fill:var(--alias)"/>
            <rect x="24" width="8" height="8" style="--ink:#20a040"/>
          </g>
          <rect x="32" width="8" height="8" style="fill:var(--missing, var(--alias))"/>
        </svg>"##,
        );
        for (index, expected) in [
            [160, 32, 64, 255],
            [32, 64, 160, 255],
            [160, 32, 64, 255],
            [32, 160, 64, 255],
            [160, 32, 64, 255],
        ]
        .iter()
        .enumerate()
        {
            let start = (4 * 40 + index * 8 + 4) * 4;
            assert_eq!(&pixels[start..start + 4], expected);
        }
    }

    #[test]
    fn svg_variable_cycles_use_outer_fallback() {
        let pixels = render(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="40" height="8">
          <style>:root {--loop:var(--loop, #ff0000); --valid:#ffffff; --hidden:var(--valid,var(--hidden));}
          rect { fill:var(--loop,#208060); }
          </style>
          <rect width="8" height="8"/>
          <rect x="8" width="8" height="8" style="fill:var(--hidden,#208060)"/>
        </svg>"##,
        );
        for x in [4, 12] {
            assert_eq!(
                &pixels[(4 * 40 + x) * 4..(4 * 40 + x) * 4 + 4],
                &[32, 128, 96, 255]
            );
        }
    }

    #[test]
    fn svg_variable_substitution_keeps_strings_and_nested_functions() {
        let result = expand(
            "'var(--literal)' rgb(var(--channels))",
            &mut |name| (name == "--channels").then(|| "10, 20, 30".to_owned()),
            0,
        )
        .unwrap();
        assert!(result.contains("var(--literal)"));
        assert!(result.contains("rgb(10, 20, 30)"));
    }
}
