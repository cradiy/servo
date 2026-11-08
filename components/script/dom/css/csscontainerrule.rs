/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

use std::cell::RefCell;

use crate::dom::bindings::codegen::Bindings::CSSContainerRuleBinding::{
    CSSContainerCondition, CSSContainerRuleMethods,
};
use crate::dom::bindings::utils::to_frozen_array;
use dom_struct::dom_struct;
use js::context::JSContext;
use js::rust::MutableHandleValue;
use script_bindings::reflector::reflect_dom_object;
use servo_arc::Arc;
use style::shared_lock::{SharedRwLockReadGuard, ToCssWithGuard};
use style::stylesheets::{ContainerRule, CssRuleType};
use style_traits::ToCss;

use super::cssconditionrule::CSSConditionRule;
use super::cssrule::SpecificCSSRule;
use super::cssstylesheet::CSSStyleSheet;
use crate::dom::bindings::root::DomRoot;
use crate::dom::bindings::str::DOMString;
use crate::dom::css::cssgroupingrule::CSSGroupingRule;
use crate::dom::window::Window;

#[dom_struct]
pub(crate) struct CSSContainerRule {
    css_condition_rule: CSSConditionRule,
    #[ignore_malloc_size_of = "Stylo"]
    #[no_trace]
    container_rule: RefCell<Arc<ContainerRule>>,
}

impl CSSContainerRule {
    fn new_inherited(
        parent_rule: Option<&CSSGroupingRule>,
        parent_stylesheet: &CSSStyleSheet,
        containerrule: Arc<ContainerRule>,
    ) -> CSSContainerRule {
        let list = containerrule.rules.clone();
        CSSContainerRule {
            css_condition_rule: CSSConditionRule::new_inherited(
                parent_rule,
                parent_stylesheet,
                list,
            ),
            container_rule: RefCell::new(containerrule),
        }
    }

    pub(crate) fn new(
        cx: &mut JSContext,
        window: &Window,
        parent_rule: Option<&CSSGroupingRule>,
        parent_stylesheet: &CSSStyleSheet,
        containerrule: Arc<ContainerRule>,
    ) -> DomRoot<CSSContainerRule> {
        reflect_dom_object(
            cx,
            Box::new(CSSContainerRule::new_inherited(
                parent_rule,
                parent_stylesheet,
                containerrule,
            )),
            window,
        )
    }

    /// <https://drafts.csswg.org/css-conditional-5/#the-csscontainerrule-interface>
    pub(crate) fn get_condition_text(&self) -> DOMString {
        self.container_rule
            .borrow()
            .conditions
            .to_css_string()
            .into()
    }

    pub(crate) fn update_rule(
        &self,
        containerrule: Arc<ContainerRule>,
        guard: &SharedRwLockReadGuard,
    ) {
        self.css_condition_rule
            .update_rules(containerrule.rules.clone(), guard);
        *self.container_rule.borrow_mut() = containerrule;
    }
}

impl SpecificCSSRule for CSSContainerRule {
    fn ty(&self) -> CssRuleType {
        CssRuleType::Container
    }

    fn get_css(&self) -> DOMString {
        let guard = self.css_condition_rule.shared_lock().read();
        self.container_rule.borrow().to_css_string(&guard).into()
    }
}

impl CSSContainerRuleMethods<crate::DomTypeHolder> for CSSContainerRule {
    fn ContainerName(&self) -> DOMString {
        let rule = self.container_rule.borrow();
        if rule.conditions.0.len() != 1 || rule.conditions.0[0].name().is_none() {
            return DOMString::new();
        }
        rule.conditions.0[0].name().to_css_string().into()
    }

    fn ContainerQuery(&self) -> DOMString {
        let rule = self.container_rule.borrow();
        if rule.conditions.0.len() != 1 {
            return DOMString::new();
        }
        rule.conditions.0[0]
            .query_condition()
            .map_or_else(DOMString::new, |condition| condition.to_css_string().into())
    }

    fn Conditions(&self, cx: &mut JSContext, retval: MutableHandleValue) {
        let rule = self.container_rule.borrow();
        let conditions: Vec<CSSContainerCondition> = rule
            .conditions
            .0
            .iter()
            .map(|condition| CSSContainerCondition {
                name: if condition.name().is_none() {
                    DOMString::new()
                } else {
                    condition.name().to_css_string().into()
                },
                query: condition
                    .query_condition()
                    .map_or_else(DOMString::new, |query| query.to_css_string().into()),
            })
            .collect();
        to_frozen_array(cx, conditions.as_slice(), retval);
    }
}
