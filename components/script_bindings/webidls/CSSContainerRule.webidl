/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

// https://drafts.csswg.org/css-conditional-5/#the-csscontainerrule-interface
dictionary CSSContainerCondition {
  required DOMString name;
  required DOMString query;
};

[Exposed=Window, Pref="layout_css_container_queries_enabled"]
interface CSSContainerRule : CSSConditionRule {
  readonly attribute DOMString containerName;
  readonly attribute DOMString containerQuery;
  readonly attribute /* FrozenArray<CSSContainerCondition> */ any conditions;
};
