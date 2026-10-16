/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! Prepared, eager style updates for a complete certified empty-leaf batch.
//!
//! Servo owns the pre-drain mutation/path certificate, snapshot validation,
//! environment preparation, empty layout-object proof and retained transaction.
//! Missing proof in this module returns None before changing any style state.

use super::*;
use crate::rule_tree::CascadeOrigin;
use crate::shared_lock::StylesheetGuards;

/// Final Selection inputs proved independent of the primary background reset.
#[derive(Debug)]
pub(super) struct SelectionBaseline {
    style: Option<Arc<ComputedValues>>,
}

/// Fully allocated style state. Preparation never mutates ElementData.
///
/// The owner must retain exclusive layout access from preparation through commit,
/// and commit only after every target and its retained transaction was accepted.
pub struct PreparedBackgroundStyle {
    old_primary: Arc<ComputedValues>,
    primary: Arc<ComputedValues>,
    revision: u64,
    family: BackgroundStyleFamily,
    cached_family: bool,
}

/// One detached, non-authoritative allocation from a previously committed pair.
/// The private constructor establishes that spare and current differ only in
/// background color. Keeping current alive makes its exact identity a durable
/// family tag; this is never a ComputedValues history chain.
#[derive(Debug)]
pub struct BackgroundStyleReuse {
    current: Arc<ComputedValues>,
    spare: Arc<ComputedValues>,
    revision: u64,
    family: BackgroundStyleFamily,
}

#[derive(Debug)]
struct BackgroundStyleFamily {
    parent: OpaqueNode,
    parent_style: Arc<ComputedValues>,
    declarations: Arc<Locked<PropertyDeclarationBlock>>,
    epoch: u64,
    viewport: Option<BackgroundViewportStamp>,
    selection: SelectionBaseline,
}

impl BackgroundStyleFamily {
    fn from_baseline(baseline: &InlineBackgroundBaseline) -> Self {
        Self {
            parent: baseline.parent,
            parent_style: baseline.parent_style.clone(),
            declarations: baseline.declarations.clone(),
            epoch: baseline.epoch,
            viewport: baseline.viewport,
            selection: SelectionBaseline {
                style: baseline.selection.as_ref().unwrap().style.clone(),
            },
        }
    }

    fn matches(&self, baseline: &InlineBackgroundBaseline) -> bool {
        let Some(selection) = baseline.selection.as_ref() else {
            return false;
        };
        self.parent == baseline.parent
            && Arc::ptr_eq(&self.parent_style, &baseline.parent_style)
            && Arc::ptr_eq(&self.declarations, &baseline.declarations)
            && self.epoch == baseline.epoch
            && self.viewport == baseline.viewport
            && match (&self.selection.style, &selection.style) {
                (None, None) => true,
                (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                _ => false,
            }
    }
}

/// Fresh document-origin environment proof; valid only for this exclusive batch.
/// Per-target ordinary-tree, live style, PDB and Selection checks remain mandatory.
pub struct BackgroundBatchProof {
    epoch: u64,
    stylist: usize,
    // Captured once under the exclusive Layout/environment preparation guard.
    viewport: Option<BackgroundViewportStamp>,
}

impl BackgroundBatchProof {
    /// Reject animation traversal, forced colors and every STYLE dependency.
    /// Caller excludes non-document scopes on every target and owns Layout.
    pub fn prepare<E: TElement>(context: &StyleContext<E>) -> Option<Self> {
        if context.shared.traversal_flags.for_animation_only()
            || context.shared.stylist.device().forced_colors().is_active()
            || !context
                .shared
                .stylist
                .document_style_snapshots_independent()
        {
            return None;
        }
        Some(Self {
            epoch: context.shared.inline_background_epoch?,
            stylist: context.shared.stylist as *const _ as usize,
            viewport: BackgroundViewportStamp::capture(context),
        })
    }

    fn matches<E: TElement>(&self, context: &StyleContext<E>) -> bool {
        Some(self.epoch) == context.shared.inline_background_epoch
            && self.stylist == context.shared.stylist as *const _ as usize
    }
}

/// One fresh parent check, shared by siblings in an exclusive complete batch.
/// Carries identity only; it neither owns a new style Arc nor extends lifetime.
#[derive(Clone, Copy)]
pub struct BackgroundParentProof {
    parent: OpaqueNode,
    style: usize,
}

impl BackgroundParentProof {
    /// Caller proves no target is another target's ancestor and keeps all parent
    /// styles unchanged until commit. No proof survives this batch.
    pub fn prepare<E: TElement>(element: E) -> Option<Self> {
        let parent = element.traversal_parent()?;
        let data = parent.borrow_data()?;
        let style = data.styles.primary.as_ref()?;
        if style.is_display_contents() {
            return None;
        }
        Some(Self {
            parent: parent.as_node().opaque(),
            style: (&**style as *const ComputedValues) as usize,
        })
    }

    fn matches<E: TElement>(&self, element: E, baseline: &InlineBackgroundBaseline) -> bool {
        element
            .traversal_parent()
            .is_some_and(|parent| parent.as_node().opaque() == self.parent)
            && self.parent == baseline.parent
            && self.style == (&*baseline.parent_style as *const ComputedValues) as usize
    }
}

impl BackgroundStyleReuse {
    /// Immutable family identity for a generation-bound leaf token. This is not
    /// a replacement for fresh epoch/PDB/parent/hint/revision checks at prepare.
    pub fn matches_primary_selection(
        &self,
        primary: &Arc<ComputedValues>,
        selection: &Arc<ComputedValues>,
        epoch: Option<u64>,
    ) -> bool {
        Some(self.family.epoch) == epoch
            && Arc::ptr_eq(&self.current, primary)
            && self
                .family
                .selection
                .style
                .as_ref()
                .is_some_and(|old| Arc::ptr_eq(old, selection))
    }

    fn matches(&self, old: &Arc<ComputedValues>, baseline: &InlineBackgroundBaseline) -> bool {
        Arc::ptr_eq(&self.current, old)
            && Arc::ptr_eq(old, &baseline.style)
            && !Arc::ptr_eq(&self.spare, old)
            && self.revision == baseline.revision
            && self.family.matches(baseline)
    }

    /// The exact fully computed primary that was published by this transaction.
    pub fn new_primary(&self) -> &Arc<ComputedValues> {
        &self.current
    }

    /// The unchanged final Selection style certified for this committed pair.
    pub fn selection(&self) -> Option<&Arc<ComputedValues>> {
        self.family.selection.style.as_ref()
    }
}

impl PreparedBackgroundStyle {
    /// Whether this preparation used its inductive immutable-family proof.
    /// This does not claim unique allocation recycling or rendering authority.
    pub fn used_cached_family(&self) -> bool {
        self.cached_family
    }

    /// The exact final primary used to certify the current layout references.
    pub fn old_primary(&self) -> &Arc<ComputedValues> {
        &self.old_primary
    }

    /// The fully computed new primary, ready before transaction submission.
    pub fn new_primary(&self) -> &Arc<ComputedValues> {
        &self.primary
    }

    /// The unchanged final Selection style, or None for ordinary primary fallback.
    pub fn selection(&self) -> Option<&Arc<ComputedValues>> {
        self.family.selection.style.as_ref()
    }
}

pub(super) fn selection_baseline<E: TElement>(
    context: &StyleContext<E>,
    pseudos: &EagerPseudoStyles,
) -> Option<SelectionBaseline> {
    selection_baseline_with_guards(pseudos, &context.shared.guards)
}

fn selection_baseline_with_guards(
    pseudos: &EagerPseudoStyles,
    guards: &StylesheetGuards,
) -> Option<SelectionBaseline> {
    if !supported_pseudos(pseudos) {
        return None;
    }
    let Some(style) = pseudos.get(&PseudoElement::Selection) else {
        return Some(SelectionBaseline { style: None });
    };
    if !style.pseudo().is_some_and(|pseudo| pseudo.is_selection())
        || !supported_independent_inputs(style)
    {
        return None;
    }
    let rules = style.rules.as_ref()?;
    // Exact origin is required: author-specified flags describe winning fields,
    // not the entire matched chain. The unchanged epoch covers UA sheet inputs.
    for node in rules.self_and_ancestors() {
        let Some(source) = node.style_source() else {
            continue;
        };
        let level = node.cascade_level();
        if level.origin() != CascadeOrigin::UA || !node.cascade_priority().flags().is_empty() {
            return None;
        }
        if source
            .read(level.guard(guards))
            .declarations()
            .iter()
            .any(|decl| {
                matches!(
                    decl,
                    crate::properties::PropertyDeclaration::CSSWideKeyword(_)
                        | crate::properties::PropertyDeclaration::WithVariables(_)
                        | crate::properties::PropertyDeclaration::Custom(_)
                )
            })
        {
            return None;
        }
    }
    Some(SelectionBaseline {
        style: Some(style.clone()),
    })
}

fn same_selection(pseudos: &EagerPseudoStyles, baseline: &SelectionBaseline) -> bool {
    if !supported_pseudos(pseudos) {
        return false;
    }
    match (
        pseudos.get(&PseudoElement::Selection),
        baseline.style.as_ref(),
    ) {
        (None, None) => true,
        (Some(current), Some(old)) => Arc::ptr_eq(current, old),
        _ => false,
    }
}

fn no_style_attribute_dependencies<E: TElement>(context: &StyleContext<E>, element: E) -> bool {
    // The current Stylist includes all applicable origins. The helper rejects
    // every nontrivial scope/container and every style-attribute dependency,
    // including nested relative and nth-of selector references.
    !context
        .shared
        .stylist
        .any_applicable_rule_data(element, |data| {
            !data.can_skip_style_attribute_snapshot_invalidation()
        })
}

/// Prepare a current primary and unchanged UA Selection without traversal.
///
/// Required caller certificate: the complete ordinary pending-restyle list has
/// only null-namespace style mutations; snapshots contain no other attr/state
/// mutation, preexisting hints/damage/dirty paths were empty before normal drain,
/// and every target is a real empty leaf with current-generation layout refs.
/// Ordinary SnapshotSetter and environment preparation precede this function.
/// No unknown shadow/scope, effect, image, focus or other paint work may bypass
/// the ordinary path. The caller must reject the whole batch on any None.
pub fn prepare_background_style<E: TElement>(
    context: &StyleContext<E>,
    element: E,
    data: &ElementData,
    expected_hint: RestyleHint,
) -> Option<PreparedBackgroundStyle> {
    prepare_background_style_with_reuse(context, element, data, expected_hint, None)
}

/// The same complete per-target proof, optionally consuming an owned scratch
/// allocation. No style authority is changed by taking or discarding scratch.
pub fn prepare_background_style_with_reuse<E: TElement>(
    context: &StyleContext<E>,
    element: E,
    data: &ElementData,
    expected_hint: RestyleHint,
    reuse: Option<BackgroundStyleReuse>,
) -> Option<PreparedBackgroundStyle> {
    prepare_background_style_inner(context, element, data, expected_hint, reuse, None)
}

/// Prepare under a fresh batch environment and parent proof. Unknown/cold family
/// uses the same full immutable-style check and ordinary allocation fallback.
pub fn prepare_background_style_in_batch<E: TElement>(
    context: &StyleContext<E>,
    element: E,
    data: &ElementData,
    expected_hint: RestyleHint,
    reuse: Option<BackgroundStyleReuse>,
    batch: &BackgroundBatchProof,
    parent: &BackgroundParentProof,
) -> Option<PreparedBackgroundStyle> {
    if !batch.matches(context) {
        return None;
    }
    prepare_background_style_inner(
        context,
        element,
        data,
        expected_hint,
        reuse,
        Some((batch, parent)),
    )
}

fn viewport_matches_batch(
    expected: Option<BackgroundViewportStamp>,
    current: Option<BackgroundViewportStamp>,
) -> bool {
    expected.is_none() || expected == current
}

fn supported_style_for_batch(
    style: &ComputedValues,
    expected: Option<BackgroundViewportStamp>,
    current: Option<BackgroundViewportStamp>,
) -> bool {
    supported_style(style)
        || (expected.is_some()
            && expected == current
            && style.flags.contains(Flags::USES_VIEWPORT_UNITS)
            && !style.is_pseudo_style()
            && supported_viewport_batch_inputs(style))
}

#[derive(Clone, Copy)]
enum BackgroundHintState {
    Applied,
    BeforeDrain,
}

impl BackgroundHintState {
    fn matches(self, actual: RestyleHint, expected: RestyleHint) -> bool {
        match self {
            Self::Applied => actual.bits() == expected.bits(),
            Self::BeforeDrain => actual.is_empty(),
        }
    }
}

struct ValidatedBackgroundStyle<'a> {
    old: &'a Arc<ComputedValues>,
    baseline: &'a InlineBackgroundBaseline,
    revision: u64,
    color: crate::color::AbsoluteColor,
}

/// Pure current-style/property proof for an internal focus-fixup read only.
/// The caller validates the complete pending list and its clean ancestry before
/// calling this. No hint, style, baseline, cache allocation or paint is changed.
/// This is deliberately not a query recascade or a retained-paint capability.
pub fn validate_background_style_before_drain<E: TElement>(
    context: &StyleContext<E>,
    element: E,
    data: &ElementData,
    batch: &BackgroundBatchProof,
    parent: &BackgroundParentProof,
) -> Option<()> {
    if !batch.matches(context) {
        return None;
    }
    validate_background_style_inner(
        context,
        element,
        data,
        RestyleHint::RESTYLE_STYLE_ATTRIBUTE,
        None,
        Some((batch, parent)),
        BackgroundHintState::BeforeDrain,
    )?;
    Some(())
}

fn validate_background_style_inner<'a, E: TElement>(
    context: &StyleContext<E>,
    element: E,
    data: &'a ElementData,
    expected_hint: RestyleHint,
    reuse: Option<&BackgroundStyleReuse>,
    batch: Option<(&BackgroundBatchProof, &BackgroundParentProof)>,
    hint_state: BackgroundHintState,
) -> Option<ValidatedBackgroundStyle<'a>> {
    let epoch = context.shared.inline_background_epoch?;
    if expected_hint.bits() != RestyleHint::RESTYLE_STYLE_ATTRIBUTE.bits()
        || !hint_state.matches(data.hint, expected_hint)
        || !data.damage.is_empty()
        || element.as_node().first_child().is_some()
        || element.shadow_root().is_some()
        || element.containing_shadow().is_some()
        || element.assigned_slot().is_some()
        || element.implemented_pseudo_element().is_some()
        || element.has_animation_only_dirty_descendants()
        || context.shared.traversal_flags.for_animation_only()
        || context.shared.stylist.device().forced_colors().is_active()
        || (batch.is_none() && !no_style_attribute_dependencies(context, element))
    {
        return None;
    }
    let old = data.styles.primary.as_ref()?;
    let baseline = data.inline_background.as_ref()?;
    let viewport = batch.and_then(|(proof, _)| proof.viewport);
    // Always revalidate the stamp, including a cached-family hit. A query or
    // non-batch caller never receives viewport-dependent admission.
    if !viewport_matches_batch(baseline.viewport, viewport) {
        return None;
    }
    let same_family = reuse.is_some_and(|reuse| reuse.matches(old, baseline));
    if baseline.epoch != epoch
        || !Arc::ptr_eq(old, &baseline.style)
        || (!same_family && !supported_style_for_batch(old, baseline.viewport, viewport))
    {
        return None;
    }
    let selection = baseline.selection.as_ref()?;
    if !same_selection(&data.styles.pseudos, selection) {
        return None;
    }
    if let Some((_, parent)) = batch {
        if !parent.matches(element, baseline) {
            return None;
        }
    } else {
        let (parent, parent_style) = parent(element)?;
        if parent != baseline.parent || !Arc::ptr_eq(&parent_style, &baseline.parent_style) {
            return None;
        }
    }
    let declarations = element.style_attribute()?;
    if !std::ptr::eq(&*declarations, &*baseline.declarations) {
        return None;
    }
    let block = declarations.read_with(context.shared.guards.author);
    if !block.only_background_color_changes_since(baseline.revision) {
        return None;
    }
    let revision = block.background_color_revision()?;
    let (declaration, importance) =
        block.get(PropertyDeclarationId::Longhand(LonghandId::BackgroundColor))?;
    if importance != Importance::Normal {
        return None;
    }
    let color = independent_background_color(declaration)?;
    // Preserve the ordinary no-paint/reuse result for compositional writes that
    // return to the exact old computed color. The whole batch takes normal style.
    if old.get_background().background_color == Color::Absolute(color) {
        return None;
    }
    // The final baseline proved the actual normal-inline winner. The same PDB
    // identity and supported mutation range preserve its priority and all other
    // declarations; parent/sheet/environment identity preserves the rule graph.
    Some(ValidatedBackgroundStyle {
        old,
        baseline,
        revision,
        color,
    })
}

fn prepare_background_style_inner<E: TElement>(
    context: &StyleContext<E>,
    element: E,
    data: &ElementData,
    expected_hint: RestyleHint,
    reuse: Option<BackgroundStyleReuse>,
    batch: Option<(&BackgroundBatchProof, &BackgroundParentProof)>,
) -> Option<PreparedBackgroundStyle> {
    let mut attribution_sample = layout_attribution::Sample::begin();
    let validated = validate_background_style_inner(
        context,
        element,
        data,
        expected_hint,
        reuse.as_ref(),
        batch,
        BackgroundHintState::Applied,
    )?;
    attribution_sample.build_started();
    let prepared = prepare_validated_style(
        validated.old,
        validated.baseline,
        validated.revision,
        validated.color,
        reuse,
    );
    attribution_sample.prepared();
    Some(prepared)
}

/// Called only after the ordinary complete target proof above. The family tag
/// proves the spare's origin inductively; it is not a substitute for any of the
/// current hint/PDB/parent/epoch/selection/leaf checks.
fn prepare_validated_style(
    old: &Arc<ComputedValues>,
    baseline: &InlineBackgroundBaseline,
    revision: u64,
    color: crate::color::AbsoluteColor,
    reuse: Option<BackgroundStyleReuse>,
) -> PreparedBackgroundStyle {
    let reuse = reuse.filter(|reuse| reuse.matches(old, baseline));
    let cached_family = reuse.is_some();
    let (old_primary, family, spare) = match reuse {
        Some(reuse) => (reuse.current, reuse.family, Some(reuse.spare)),
        None => (
            old.clone(),
            BackgroundStyleFamily::from_baseline(baseline),
            None,
        ),
    };
    let recycled = spare.and_then(|spare| recolor_unique_spare(spare, color));
    let primary = recycled.unwrap_or_else(|| {
        let mut values = (**old).clone();
        values.mutate_background().background_color = Color::Absolute(color);
        Arc::new(values)
    });
    PreparedBackgroundStyle {
        old_primary,
        primary,
        revision,
        family,
        cached_family,
    }
}

fn recolor_unique_spare(
    mut spare: Arc<ComputedValues>,
    color: crate::color::AbsoluteColor,
) -> Option<Arc<ComputedValues>> {
    // Both allocations must be unique before even the color is written.
    // A query/cache/external Arc is an ordinary allocation-path miss.
    let background = Arc::get_mut(&mut spare)?.try_mutate_unique_background()?;
    background.background_color = Color::Absolute(color);
    Some(spare)
}

/// Commit a fully prepared style after positive retained-transaction ACK.
///
/// No validation, allocation, cascade, callbacks, or locks occur here. The caller
/// owns the unchanged ElementData exclusively since preparation and commits all
/// matching layout references and certified dirty paths in the same opportunity.
/// The exact proof consumes only the primary background repaint; no other damage
/// or hint is admitted. WAS_RESTYLED remains as in the ordinary repair path.
pub fn commit_background_style(
    data: &mut ElementData,
    prepared: PreparedBackgroundStyle,
) -> BackgroundStyleReuse {
    let PreparedBackgroundStyle {
        old_primary,
        primary,
        revision,
        family,
        cached_family: _,
    } = prepared;
    // Preserve the exact set_styles flag semantics while retaining the unchanged
    // eager-pseudo allocation and final Selection identity in place.
    data.flags.set(
        crate::data::ElementDataFlags::PRIMARY_STYLE_REUSED_VIA_RULE_NODE,
        false,
    );
    data.styles.primary = Some(primary.clone());
    data.set_restyled();
    data.hint = RestyleHint::empty();
    // This existing box and its fixed provenance inputs were certified above.
    // No new box, parent/declaration/pseudo clone or delayed style debt is needed.
    let baseline = data.inline_background.as_mut().unwrap();
    baseline.style = primary.clone();
    baseline.revision = revision;
    baseline.previous = None;
    BackgroundStyleReuse {
        current: primary,
        spare: old_primary,
        revision,
        family,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::applicable_declarations::CascadePriority;
    use crate::color::AbsoluteColor;
    use crate::properties::style_structs::Font;
    use crate::properties::{PropertyDeclaration, parse_style_attribute};
    use crate::rule_tree::{CascadeLevel, RuleCascadeFlags, RuleTree, StrongRuleNode, StyleSource};
    use crate::shared_lock::SharedRwLock;
    use crate::stylesheets::layer_rule::LayerOrder;
    use crate::stylesheets::{CssRuleType, UrlExtraData};
    use selectors::matching::QuirksMode;

    fn values(
        color: AbsoluteColor,
        rules: Option<StrongRuleNode>,
        pseudo: bool,
    ) -> Arc<ComputedValues> {
        let initial = ComputedValues::initial_values_with_font_override(Font::initial_values());
        let initial = if pseudo {
            initial.clone_with_flags(Flags::empty(), Some(&PseudoElement::Selection))
        } else {
            initial
        };
        let mut values = (*initial).clone();
        values.rules = rules;
        values.mutate_background().background_color = Color::Absolute(color);
        Arc::new(values)
    }

    #[test]
    fn focus_background_proof_requires_clean_predrain_hint_without_relaxing_applied_hint() {
        let style = RestyleHint::RESTYLE_STYLE_ATTRIBUTE;
        assert!(BackgroundHintState::BeforeDrain.matches(RestyleHint::empty(), style));
        assert!(!BackgroundHintState::BeforeDrain.matches(style, style));
        assert!(!BackgroundHintState::BeforeDrain.matches(RestyleHint::RESTYLE_SELF, style));
        assert!(BackgroundHintState::Applied.matches(style, style));
        assert!(!BackgroundHintState::Applied.matches(RestyleHint::empty(), style));
        assert!(!BackgroundHintState::Applied.matches(style | RestyleHint::RESTYLE_SELF, style));
    }

    fn viewport_test_style(extra: Flags) -> Arc<ComputedValues> {
        let lock = SharedRwLock::new();
        let tree = RuleTree::new();
        let rules = tree.insert_ordered_rules(
            [(
                source(&lock, "background-color:black"),
                priority(CascadeOrigin::UA),
            )]
            .into_iter(),
        );
        let mut style = (*values(AbsoluteColor::BLACK, Some(rules), false)).clone();
        style.flags.insert(Flags::USES_VIEWPORT_UNITS | extra);
        Arc::new(style)
    }

    #[test]
    fn stable_viewport_batch_exact_stamp_admits_without_relaxing_ordinary_guard() {
        let style = viewport_test_style(Flags::empty());
        let stamp = BackgroundViewportStamp::from_values(7, [1280., 720., 1920., 1080., 1.5]);
        assert!(stamp.is_some());
        assert!(!supported_style(&style));
        assert!(supported_style_for_batch(&style, stamp, stamp));
        assert!(!supported_style_for_batch(&style, stamp, None));
        assert!(!supported_style_for_batch(&style, None, stamp));
    }

    #[test]
    fn stable_viewport_batch_resize_dpr_screen_and_epoch_changes_reject_until_fresh_baseline() {
        let style = viewport_test_style(Flags::empty());
        let values = [1280., 720., 1920., 1080., 1.5];
        let old = BackgroundViewportStamp::from_values(7, values);
        for index in 0..5 {
            let mut changed = values;
            changed[index] += 1.;
            let current = BackgroundViewportStamp::from_values(8, changed);
            assert!(!viewport_matches_batch(old, current));
            assert!(!supported_style_for_batch(&style, old, current));
            assert!(supported_style_for_batch(&style, current, current));
        }
        let media_epoch = BackgroundViewportStamp::from_values(8, values);
        assert!(!supported_style_for_batch(&style, old, media_epoch));
    }

    #[test]
    fn stable_viewport_batch_unknown_nonfinite_and_other_dependencies_stay_rejected() {
        for value in [0., -1., f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            for index in 0..5 {
                let mut values = [1280., 720., 1920., 1080., 1.5];
                values[index] = value;
                assert!(BackgroundViewportStamp::from_values(7, values).is_none());
            }
        }
        let stamp = BackgroundViewportStamp::from_values(7, [1280., 720., 1920., 1080., 1.5]);
        for flags in [
            Flags::USES_VIEWPORT_UNITS_ON_CONTAINER_QUERIES,
            Flags::USES_CONTAINER_UNITS,
            Flags::DEPENDS_ON_SELF_FONT_METRICS,
            Flags::INHERITS_RESET_STYLE,
        ] {
            assert!(!supported_style_for_batch(
                &viewport_test_style(flags),
                stamp,
                stamp
            ));
        }
    }

    #[test]
    fn stable_viewport_batch_family_seal_survives_commit_but_not_resize_or_abandoned_prepare() {
        let mut data = reuse_fixture();
        let epoch = data.inline_background.as_ref().unwrap().epoch;
        let stamp = BackgroundViewportStamp::from_values(epoch, [1280., 720., 1920., 1080., 1.5]);
        data.inline_background.as_mut().unwrap().viewport = stamp;
        let first = stage_reuse(&data, AbsoluteColor::WHITE, None);
        let reuse = commit_background_style(&mut data, first);
        assert_eq!(reuse.family.viewport, stamp);
        assert!(reuse.matches(
            data.styles.primary.as_ref().unwrap(),
            data.inline_background.as_ref().unwrap()
        ));
        let old = data.styles.primary.as_ref().unwrap().clone();
        drop(stage_reuse(&data, AbsoluteColor::BLACK, Some(reuse)));
        assert!(Arc::ptr_eq(data.styles.primary.as_ref().unwrap(), &old));
        assert_eq!(data.inline_background.as_ref().unwrap().viewport, stamp);
        let resized =
            BackgroundViewportStamp::from_values(epoch + 1, [1440., 900., 1920., 1080., 1.5]);
        assert!(!viewport_matches_batch(stamp, resized));
        assert!(!viewport_matches_batch(stamp, None));
        let fresh = stage_reuse(&data, AbsoluteColor::BLACK, None);
        let reuse = commit_background_style(&mut data, fresh);
        data.inline_background.as_mut().unwrap().viewport = resized;
        assert!(!reuse.matches(
            data.styles.primary.as_ref().unwrap(),
            data.inline_background.as_ref().unwrap()
        ));
    }

    #[test]
    fn stable_viewport_batch_actual_device_setters_and_commit_preserve_viewport_flags() {
        #[derive(Debug)]
        struct NoFontQueries;
        impl crate::device::servo::FontMetricsProvider for NoFontQueries {
            fn query_font_metrics(
                &self,
                _: bool,
                _: &Font,
                _: crate::values::computed::CSSPixelLength,
                _: crate::values::specified::font::QueryFontMetricsFlags,
            ) -> crate::font_metrics::FontMetrics {
                panic!("viewport capture must not query font metrics")
            }
            fn base_size_for_generic(
                &self,
                _: crate::values::computed::font::GenericFontFamily,
            ) -> crate::values::computed::Length {
                panic!("viewport capture must not query font size")
            }
        }
        let mut device = crate::device::Device::new(
            crate::media_queries::MediaType::screen(),
            QuirksMode::NoQuirks,
            euclid::Size2D::new(400., 240.),
            euclid::Size2D::new(800., 480.),
            euclid::Scale::new(2.),
            Box::new(NoFontQueries),
            ComputedValues::initial_values_with_font_override(Font::initial_values()),
            crate::queries::values::PrefersColorScheme::Light,
            crate::servo::media_features::PointerCapabilities::default(),
            crate::servo::media_features::PointerCapabilities::default(),
        );
        let old = BackgroundViewportStamp::for_device(7, &device).unwrap();
        device.set_viewport_size(euclid::Size2D::new(600., 320.));
        let resized = BackgroundViewportStamp::for_device(7, &device).unwrap();
        assert!(!viewport_matches_batch(Some(old), Some(resized)));
        device.set_device_pixel_ratio(euclid::Scale::new(1.5));
        let dpr = BackgroundViewportStamp::for_device(7, &device).unwrap();
        assert!(!viewport_matches_batch(Some(resized), Some(dpr)));
        device.set_device_size(euclid::Size2D::new(1200., 640.));
        let screen = BackgroundViewportStamp::for_device(7, &device).unwrap();
        assert!(!viewport_matches_batch(Some(dpr), Some(screen)));
        let renewed = BackgroundViewportStamp::for_device(8, &device).unwrap();
        assert!(!viewport_matches_batch(Some(screen), Some(renewed)));
        let viewport_style = viewport_test_style(Flags::empty());
        assert!(!supported_style_for_batch(
            &viewport_style,
            Some(old),
            Some(renewed)
        ));
        assert!(supported_style_for_batch(
            &viewport_style,
            Some(renewed),
            Some(renewed)
        ));
        // Exercise the actual complete-style constructor and positive commit,
        // not a synthetic counter. Viewport geometry flags must survive both.
        let mut data = reuse_fixture();
        let mut primary = (**data.styles.primary.as_ref().unwrap()).clone();
        primary.flags.insert(Flags::USES_VIEWPORT_UNITS);
        let primary = Arc::new(primary);
        data.styles.primary = Some(primary.clone());
        let baseline = data.inline_background.as_mut().unwrap();
        baseline.style = primary;
        baseline.epoch = 8;
        baseline.viewport = Some(renewed);
        let prepared = stage_reuse(&data, AbsoluteColor::WHITE, None);
        assert!(
            prepared
                .new_primary()
                .flags
                .contains(Flags::USES_VIEWPORT_UNITS)
        );
        let reuse = commit_background_style(&mut data, prepared);
        assert!(
            data.styles
                .primary
                .as_ref()
                .unwrap()
                .flags
                .contains(Flags::USES_VIEWPORT_UNITS)
        );
        assert_eq!(reuse.family.viewport, Some(renewed));
        assert_eq!(
            data.inline_background.as_ref().unwrap().viewport,
            Some(renewed)
        );
    }

    fn source(lock: &SharedRwLock, css: &str) -> StyleSource {
        let url = UrlExtraData(Arc::new(url::Url::parse("about:blank").unwrap()));
        let block =
            parse_style_attribute(css, &url, None, QuirksMode::NoQuirks, CssRuleType::Style);
        assert!(!block.declarations().is_empty());
        StyleSource::from_declarations(Arc::new(lock.wrap(block)))
    }

    fn priority(origin: CascadeOrigin) -> CascadePriority {
        CascadePriority::new(
            CascadeLevel::new(origin),
            LayerOrder::root(),
            RuleCascadeFlags::empty(),
        )
    }

    fn pseudos(style: Arc<ComputedValues>) -> EagerPseudoStyles {
        let mut pseudos = EagerPseudoStyles::default();
        pseudos.set(&PseudoElement::Selection, style);
        pseudos
    }

    #[test]
    fn eager_background_batch_selection_proof_requires_entire_ua_chain_and_exact_final_arc() {
        let lock = SharedRwLock::new();
        let tree = RuleTree::new();
        let rules = tree.insert_ordered_rules(
            [(
                source(&lock, "background:rgba(176,214,255,1)"),
                priority(CascadeOrigin::UA),
            )]
            .into_iter(),
        );
        let style = values(
            AbsoluteColor::srgb_legacy(176, 214, 255, 1.),
            Some(rules.clone()),
            true,
        );
        let guard = lock.read();
        let guards = StylesheetGuards {
            author: &guard,
            ua_or_user: &guard,
        };
        let input = pseudos(style.clone());
        let proof = selection_baseline_with_guards(&input, &guards).unwrap();
        assert!(same_selection(&input, &proof));
        let equivalent_new_arc = Arc::new((*style).clone());
        assert!(!same_selection(&pseudos(equivalent_new_arc), &proof));
        drop(guard);
        // A non-background author declaration still makes the whole matched
        // chain ineligible, regardless of which background field won.
        let authored = tree.insert_ordered_rules(
            [
                (
                    source(&lock, "background:rgba(176,214,255,1)"),
                    priority(CascadeOrigin::UA),
                ),
                (
                    source(&lock, "color:white"),
                    priority(CascadeOrigin::Author),
                ),
            ]
            .into_iter(),
        );
        let author_style = values(
            AbsoluteColor::srgb_legacy(176, 214, 255, 1.),
            Some(authored),
            true,
        );
        let guard = lock.read();
        let guards = StylesheetGuards {
            author: &guard,
            ua_or_user: &guard,
        };
        assert!(selection_baseline_with_guards(&pseudos(author_style), &guards).is_none());
    }

    #[test]
    fn eager_background_batch_selection_reset_variables_and_generated_pseudos_reject() {
        let lock = SharedRwLock::new();
        let tree = RuleTree::new();
        for css in [
            "background-color:inherit",
            "background-color:var(--selection)",
            "--selection:red;background-color:red",
        ] {
            let rules = tree.insert_ordered_rules(
                [(source(&lock, css), priority(CascadeOrigin::UA))].into_iter(),
            );
            let style = values(AbsoluteColor::srgb_legacy(255, 0, 0, 1.), Some(rules), true);
            let guard = lock.read();
            let guards = StylesheetGuards {
                author: &guard,
                ua_or_user: &guard,
            };
            assert!(
                selection_baseline_with_guards(&pseudos(style), &guards).is_none(),
                "{css}"
            );
        }
        let rules = tree.insert_ordered_rules(
            [(
                source(&lock, "background-color:red"),
                priority(CascadeOrigin::UA),
            )]
            .into_iter(),
        );
        let style = values(AbsoluteColor::srgb_legacy(255, 0, 0, 1.), Some(rules), true);
        let mut reset = (*style).clone();
        reset.flags.insert(Flags::INHERITS_RESET_STYLE);
        let guard = lock.read();
        let guards = StylesheetGuards {
            author: &guard,
            ua_or_user: &guard,
        };
        assert!(selection_baseline_with_guards(&pseudos(Arc::new(reset)), &guards).is_none());
        let mut generated = pseudos(style.clone());
        generated.set(&PseudoElement::Before, style);
        assert!(selection_baseline_with_guards(&generated, &guards).is_none());
        let absent =
            selection_baseline_with_guards(&EagerPseudoStyles::default(), &guards).unwrap();
        assert!(!same_selection(&generated, &absent));
    }

    #[test]
    fn eager_background_batch_abandon_preserves_state_and_commit_installs_complete_styles() {
        let black = AbsoluteColor::srgb_legacy(0, 0, 0, 1.);
        let blue = AbsoluteColor::srgb_legacy(0, 0, 255, 1.);
        let old = values(black, None, false);
        let new = values(blue, None, false);
        let selected = values(black, None, true);
        let lock = SharedRwLock::new();
        let declarations = Arc::new(lock.wrap(PropertyDeclarationBlock::with_one(
            PropertyDeclaration::BackgroundColor(black.into()),
            Importance::Normal,
        )));
        let baseline = |style: Arc<ComputedValues>, revision| {
            Box::new(InlineBackgroundBaseline {
                style,
                parent: OpaqueNode(1),
                parent_style: old.clone(),
                declarations: declarations.clone(),
                revision,
                epoch: 4,
                viewport: None,
                previous: None,
                selection: Some(SelectionBaseline {
                    style: Some(selected.clone()),
                }),
            })
        };
        let mut data = ElementData::default();
        data.styles.primary = Some(old.clone());
        data.styles.pseudos = pseudos(selected.clone());
        data.hint = RestyleHint::RESTYLE_STYLE_ATTRIBUTE;
        data.inline_background = Some(baseline(old.clone(), 2));
        let prepared = || PreparedBackgroundStyle {
            cached_family: false,
            old_primary: old.clone(),
            primary: new.clone(),
            revision: 3,
            family: BackgroundStyleFamily::from_baseline(&baseline(old.clone(), 2)),
        };
        drop(prepared());
        assert!(Arc::ptr_eq(data.styles.primary.as_ref().unwrap(), &old));
        assert_eq!(
            data.hint.bits(),
            RestyleHint::RESTYLE_STYLE_ATTRIBUTE.bits()
        );
        assert_eq!(data.inline_background.as_ref().unwrap().revision, 2);
        assert!(!data.is_restyle());
        commit_background_style(&mut data, prepared());
        assert!(Arc::ptr_eq(data.styles.primary.as_ref().unwrap(), &new));
        assert!(Arc::ptr_eq(
            data.styles.pseudos.get(&PseudoElement::Selection).unwrap(),
            &selected
        ));
        assert_eq!(
            old.get_background().background_color,
            Color::Absolute(black)
        );
        assert_eq!(new.get_background().background_color, Color::Absolute(blue));
        assert!(old.same_except_background_color(&new));
        assert!(data.hint.is_empty() && data.damage.is_empty() && data.is_restyle());
        assert_eq!(data.inline_background.as_ref().unwrap().revision, 3);
        assert!(data.inline_background.as_ref().unwrap().previous.is_none());
    }

    fn reuse_fixture() -> ElementData {
        let black = AbsoluteColor::srgb_legacy(0, 0, 0, 1.);
        let old = values(black, None, false);
        // A real child's parent must not accidentally keep its retired CV alive.
        let parent_style = values(black, None, false);
        let selected = values(black, None, true);
        let lock = SharedRwLock::new();
        let declarations = Arc::new(lock.wrap(PropertyDeclarationBlock::with_one(
            PropertyDeclaration::BackgroundColor(black.into()),
            Importance::Normal,
        )));
        let mut data = ElementData::default();
        data.styles.primary = Some(old.clone());
        data.styles.pseudos = pseudos(selected.clone());
        data.inline_background = Some(Box::new(InlineBackgroundBaseline {
            style: old,
            parent: OpaqueNode(100),
            parent_style,
            declarations,
            revision: 1,
            epoch: 4,
            viewport: None,
            previous: None,
            selection: Some(SelectionBaseline {
                style: Some(selected),
            }),
        }));
        data
    }

    fn stage_reuse(
        data: &ElementData,
        color: AbsoluteColor,
        reuse: Option<BackgroundStyleReuse>,
    ) -> PreparedBackgroundStyle {
        let baseline = data.inline_background.as_ref().unwrap();
        prepare_validated_style(
            data.styles.primary.as_ref().unwrap(),
            baseline,
            baseline.revision + 1,
            color,
            reuse,
        )
    }

    fn cv_identity(style: &Arc<ComputedValues>) -> *const ComputedValues {
        &**style
    }

    #[test]
    fn unified_background_family_proof_requires_live_primary_selection_and_epoch() {
        let mut data = reuse_fixture();
        let cold = stage_reuse(&data, AbsoluteColor::WHITE, None);
        assert!(!cold.used_cached_family());
        let reuse = commit_background_style(&mut data, cold);
        let current = data.styles.primary.as_ref().unwrap();
        let selection = reuse.selection().unwrap();
        assert!(reuse.matches_primary_selection(current, selection, Some(4)));
        assert!(!reuse.matches_primary_selection(current, selection, Some(5)));
        assert!(!reuse.matches_primary_selection(current, selection, None));
        let alias = Arc::new((**current).clone());
        assert!(!reuse.matches_primary_selection(&alias, selection, Some(4)));
        let selected_alias = Arc::new((**selection).clone());
        assert!(!reuse.matches_primary_selection(current, &selected_alias, Some(4)));
        let hot = stage_reuse(&data, AbsoluteColor::BLACK, Some(reuse));
        assert!(hot.used_cached_family());
        assert!(
            hot.old_primary()
                .same_except_background_color(hot.new_primary())
        );
    }

    #[test]
    fn eager_background_reuse_alternates_two_unique_allocations_and_preserves_complete_style() {
        let black = AbsoluteColor::srgb_legacy(0, 0, 0, 1.);
        let blue = AbsoluteColor::srgb_legacy(0, 0, 255, 1.);
        let mut data = reuse_fixture();
        let first_identity = cv_identity(data.styles.primary.as_ref().unwrap());
        let first = stage_reuse(&data, blue, None);
        let second_identity = cv_identity(first.new_primary());
        assert_ne!(first_identity, second_identity);
        let mut reuse = commit_background_style(&mut data, first);
        for (color, expected) in [
            (black, first_identity),
            (blue, second_identity),
            (black, first_identity),
            (blue, second_identity),
        ] {
            let prepared = stage_reuse(&data, color, Some(reuse));
            assert_eq!(cv_identity(prepared.new_primary()), expected);
            assert!(
                prepared
                    .old_primary()
                    .same_except_background_color(prepared.new_primary())
            );
            assert_eq!(
                prepared.new_primary().get_background().background_color,
                Color::Absolute(color)
            );
            reuse = commit_background_style(&mut data, prepared);
            assert!(reuse.matches(
                data.styles.primary.as_ref().unwrap(),
                data.inline_background.as_ref().unwrap()
            ));
        }
    }

    #[test]
    fn eager_background_reuse_external_cv_and_shared_background_are_distinct_safe_misses() {
        let black = AbsoluteColor::srgb_legacy(0, 0, 0, 1.);
        let blue = AbsoluteColor::srgb_legacy(0, 0, 255, 1.);
        for shared_outer in [true, false] {
            let direct_spare = values(black, None, false);
            let direct_alias = if shared_outer {
                direct_spare.clone()
            } else {
                Arc::new((*direct_spare).clone())
            };
            assert!(recolor_unique_spare(direct_spare, AbsoluteColor::WHITE).is_none());
            assert_eq!(
                direct_alias.get_background().background_color,
                Color::Absolute(black)
            );
            let mut data = reuse_fixture();
            let first = stage_reuse(&data, blue, None);
            let mut reuse = commit_background_style(&mut data, first);
            let external = if shared_outer {
                reuse.spare.clone()
            } else {
                Arc::new((*reuse.spare).clone())
            };
            assert_eq!(Arc::get_mut(&mut reuse.spare).is_none(), shared_outer);
            if !shared_outer {
                assert!(
                    Arc::get_mut(&mut reuse.spare)
                        .unwrap()
                        .try_mutate_unique_background()
                        .is_none()
                );
            }
            let old_background = external.get_background() as *const _;
            let prepared = stage_reuse(&data, AbsoluteColor::WHITE, Some(reuse));
            assert!(!Arc::ptr_eq(prepared.new_primary(), &external));
            assert_ne!(
                prepared.new_primary().get_background() as *const _,
                old_background
            );
            assert_eq!(
                external.get_background().background_color,
                Color::Absolute(black)
            );
            assert_eq!(
                prepared.new_primary().get_background().background_color,
                Color::Absolute(AbsoluteColor::WHITE)
            );
            assert!(
                prepared
                    .old_primary()
                    .same_except_background_color(prepared.new_primary())
            );
            let reuse = commit_background_style(&mut data, prepared);
            drop(external);
            // A miss still establishes a valid next committed pair.
            let prepared = stage_reuse(&data, blue, Some(reuse));
            assert_eq!(
                prepared.new_primary().get_background().background_color,
                Color::Absolute(blue)
            );
        }
    }

    #[test]
    fn eager_background_reuse_rejects_stale_current_revision_parent_epoch_pdb_and_selection_family()
    {
        let blue = AbsoluteColor::srgb_legacy(0, 0, 255, 1.);
        for mismatch in 0..8 {
            let mut data = reuse_fixture();
            let first = stage_reuse(&data, blue, None);
            let reuse = commit_background_style(&mut data, first);
            let baseline = data.inline_background.as_mut().unwrap();
            match mismatch {
                0 => baseline.revision += 1,
                1 => baseline.parent = OpaqueNode(101),
                2 => baseline.parent_style = Arc::new((*baseline.parent_style).clone()),
                3 => baseline.epoch += 1,
                4 => {
                    let lock = SharedRwLock::new();
                    baseline.declarations =
                        Arc::new(lock.wrap(PropertyDeclarationBlock::with_one(
                            PropertyDeclaration::BackgroundColor(blue.into()),
                            Importance::Normal,
                        )));
                },
                5 => baseline.selection.as_mut().unwrap().style = Some(values(blue, None, true)),
                6 => baseline.selection = None,
                7 => {
                    let mut changed = (**data.styles.primary.as_ref().unwrap()).clone();
                    changed.mutate_effects().opacity = 0.5;
                    let changed = Arc::new(changed);
                    baseline.style = changed.clone();
                    data.styles.primary = Some(changed);
                },
                _ => unreachable!(),
            }
            assert!(
                !reuse.matches(
                    data.styles.primary.as_ref().unwrap(),
                    data.inline_background.as_ref().unwrap()
                ),
                "mismatch {mismatch}"
            );
            // Missing Selection is rejected by the public proof before this helper.
            if mismatch != 6 {
                let prepared = stage_reuse(&data, AbsoluteColor::WHITE, Some(reuse));
                assert!(
                    prepared
                        .old_primary()
                        .same_except_background_color(prepared.new_primary())
                );
                if mismatch == 7 {
                    assert_eq!(prepared.new_primary().get_effects().opacity, 0.5);
                }
            }
        }
    }

    #[test]
    fn eager_background_reuse_abandon_keeps_authority_and_commit_retains_baseline_pseudos_and_flags()
     {
        let blue = AbsoluteColor::srgb_legacy(0, 0, 255, 1.);
        let mut data = reuse_fixture();
        let baseline_address = &**data.inline_background.as_ref().unwrap() as *const _;
        let pseudo_address = data.styles.pseudos.as_array() as *const _;
        let selected = data
            .styles
            .pseudos
            .get(&PseudoElement::Selection)
            .unwrap()
            .clone();
        let old = data.styles.primary.as_ref().unwrap().clone();
        data.flags
            .insert(crate::data::ElementDataFlags::PRIMARY_STYLE_REUSED_VIA_RULE_NODE);
        data.hint = RestyleHint::RESTYLE_STYLE_ATTRIBUTE;
        drop(stage_reuse(&data, blue, None));
        assert!(Arc::ptr_eq(data.styles.primary.as_ref().unwrap(), &old));
        assert_eq!(data.inline_background.as_ref().unwrap().revision, 1);
        assert!(!data.is_restyle());
        let prepared = stage_reuse(&data, blue, None);
        let reuse = commit_background_style(&mut data, prepared);
        assert_eq!(
            &**data.inline_background.as_ref().unwrap() as *const _,
            baseline_address
        );
        assert_eq!(data.styles.pseudos.as_array() as *const _, pseudo_address);
        assert!(Arc::ptr_eq(
            data.styles.pseudos.get(&PseudoElement::Selection).unwrap(),
            &selected
        ));
        assert!(
            !data
                .flags
                .contains(crate::data::ElementDataFlags::PRIMARY_STYLE_REUSED_VIA_RULE_NODE)
        );
        assert!(data.is_restyle() && data.hint.is_empty() && data.damage.is_empty());
        assert_eq!(data.inline_background.as_ref().unwrap().revision, 2);
        assert!(data.inline_background.as_ref().unwrap().previous.is_none());
        assert!(Arc::ptr_eq(&reuse.spare, &old));
    }

    #[test]
    fn eager_background_reuse_abandoned_recycled_prepare_preserves_current_then_recovers() {
        let mut data = reuse_fixture();
        let first = stage_reuse(&data, AbsoluteColor::WHITE, None);
        let reuse = commit_background_style(&mut data, first);
        let current = data.styles.primary.as_ref().unwrap().clone();
        let revision = data.inline_background.as_ref().unwrap().revision;
        let prepared = stage_reuse(&data, AbsoluteColor::BLACK, Some(reuse));
        assert_eq!(
            prepared.new_primary().get_background().background_color,
            Color::Absolute(AbsoluteColor::BLACK)
        );
        drop(prepared);
        assert!(Arc::ptr_eq(data.styles.primary.as_ref().unwrap(), &current));
        assert_eq!(
            current.get_background().background_color,
            Color::Absolute(AbsoluteColor::WHITE)
        );
        assert_eq!(data.inline_background.as_ref().unwrap().revision, revision);
        let fresh = stage_reuse(&data, AbsoluteColor::BLACK, None);
        let reuse = commit_background_style(&mut data, fresh);
        assert_eq!(
            reuse.new_primary().get_background().background_color,
            Color::Absolute(AbsoluteColor::BLACK)
        );
    }
}

// Disposable, opt-in attribution. No frame/readiness or style decision uses this data.
mod layout_attribution {
    use std::cell::RefCell;
    use std::fmt::Debug;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    const CAP: u64 = 64;
    const ACCEPTED_CAP: u64 = 8;
    static ENABLED: OnceLock<bool> = OnceLock::new();
    static REQUESTS: AtomicU64 = AtomicU64::new(0);
    static NONEMPTY: AtomicU64 = AtomicU64::new(0);
    static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
    static ACCEPTED: AtomicU64 = AtomicU64::new(0);
    static ELIGIBLE: AtomicU64 = AtomicU64::new(0);
    static EMITTED: AtomicU64 = AtomicU64::new(0);
    static OMITTED: AtomicU64 = AtomicU64::new(0);
    static LOST: AtomicU64 = AtomicU64::new(0);
    thread_local! { static CONTEXT: RefCell<Vec<Context>> = const { RefCell::new(Vec::new()) }; }

    pub(super) fn enabled() -> bool {
        *ENABLED.get_or_init(|| {
            std::env::var("LOOM_CERTIFIED_LAYOUT_PROBE").as_deref() == Ok("1")
                && std::env::var("LOOM_PACING_ORACLE").as_deref() == Ok("1")
                && std::env::var("LOOM_PACING_PROFILE").as_deref() == Ok("app")
        })
    }
    fn utc_us() -> u128 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_micros())
            .unwrap_or(0)
    }
    pub(super) fn sampled(index: u64, attempt: u64) -> bool {
        index % 64 == attempt % 64
    }
    pub(super) fn admission(
        nonempty: u64,
        elapsed_ms: f64,
        companion: bool,
        accepted: Option<u64>,
    ) -> bool {
        (nonempty > 0 && nonempty <= 16)
            || elapsed_ms >= 5.0
            || companion
            || accepted.is_some_and(|n| n <= ACCEPTED_CAP)
    }
    #[derive(Default)]
    struct Samples {
        targets: u64,
        sampled: u64,
        prepared: u64,
        rejected: u64,
        proof_ms: f64,
        build_ms: f64,
        total_ms: f64,
    }
    struct Context {
        ordinal: u64,
        start: Instant,
        utc_start: u128,
        pipeline: String,
        goal: String,
        epoch: String,
        pending: usize,
        has_restyle: bool,
        nonempty: u64,
        companion: bool,
        attempt: Option<u64>,
        accepted: Option<u64>,
        nodes: u32,
        marks: [Option<Instant>; 8],
        ack: Option<bool>,
        samples: Samples,
        sampling_enabled: bool,
    }
    /// Diagnostic guard returned through the existing prepared-style API.
    pub struct Guard {
        ordinal: Option<u64>,
    }
    /// Marks restyle scope exit after unchanged implicit local drops.
    pub struct Exit;
    impl Drop for Exit {
        fn drop(&mut self) {
            mark("restyle_exit");
        }
    }
    pub(super) fn begin(
        pipeline: &impl Debug,
        goal: &impl Debug,
        epoch: &impl Debug,
        has_restyle: bool,
        pending: usize,
    ) -> Guard {
        if !enabled() {
            return Guard { ordinal: None };
        }
        let start = Instant::now();
        let utc_start = utc_us();
        let ordinal = REQUESTS.fetch_add(1, Ordering::Relaxed) + 1;
        let nonempty = if pending > 0 {
            NONEMPTY.fetch_add(1, Ordering::Relaxed) + 1
        } else {
            0
        };
        let accepted = ACCEPTED.load(Ordering::Relaxed);
        let context = Context {
            ordinal,
            start,
            utc_start,
            pipeline: format!("{pipeline:?}"),
            goal: format!("{goal:?}"),
            epoch: format!("{epoch:?}"),
            pending,
            has_restyle,
            nonempty,
            companion: accepted > 0 && accepted < ACCEPTED_CAP,
            attempt: None,
            accepted: None,
            nodes: 0,
            marks: [None; 8],
            ack: None,
            samples: Samples::default(),
            sampling_enabled: false,
        };
        CONTEXT.with(|stack| stack.borrow_mut().push(context));
        Guard {
            ordinal: Some(ordinal),
        }
    }
    pub(super) fn mark(name: &'static str) {
        if !enabled() {
            return;
        }
        CONTEXT.with(|stack| {
            let mut stack = stack.borrow_mut();
            let Some(c) = stack.last_mut() else {
                return;
            };
            let slot = match name {
                "capture_begin" => 0,
                "prepare_begin" => 1,
                "prepare_end" => 2,
                "guards_released" => 3,
                "ack_begin" => 4,
                "ack_end" => 5,
                "commit_end" => 6,
                "restyle_exit" => 7,
                _ => return,
            };
            if slot == 1 {
                c.attempt = Some(ATTEMPTS.fetch_add(1, Ordering::Relaxed) + 1);
                c.sampling_enabled = ACCEPTED.load(Ordering::Relaxed) < ACCEPTED_CAP;
            }
            c.marks[slot] = Some(Instant::now());
        });
    }
    pub(super) fn ack(value: bool) {
        mark("ack_end");
        if enabled() {
            CONTEXT.with(|stack| {
                if let Some(c) = stack.borrow_mut().last_mut() {
                    c.ack = Some(value);
                }
            });
        }
    }
    pub(super) fn accepted(nodes: u32) {
        if !enabled() {
            return;
        }
        let ordinal = ACCEPTED.fetch_add(1, Ordering::Relaxed) + 1;
        CONTEXT.with(|stack| {
            if let Some(c) = stack.borrow_mut().last_mut() {
                c.accepted = Some(ordinal);
                c.nodes = nodes;
            }
        });
    }
    fn between(a: Option<Instant>, b: Option<Instant>) -> Option<f64> {
        a.zip(b)
            .map(|(a, b)| b.saturating_duration_since(a).as_secs_f64() * 1000.0)
    }
    impl Drop for Guard {
        fn drop(&mut self) {
            let Some(ordinal) = self.ordinal else {
                return;
            };
            let end = Instant::now();
            let utc_end = utc_us();
            let context = CONTEXT.with(|stack| {
                let Ok(mut stack) = stack.try_borrow_mut() else {
                    return None;
                };
                let index = stack.iter().rposition(|c| c.ordinal == ordinal)?;
                Some(stack.remove(index))
            });
            let Some(c) = context else {
                LOST.fetch_add(1, Ordering::Relaxed);
                return;
            };
            let elapsed_ms = end.saturating_duration_since(c.start).as_secs_f64() * 1000.0;
            if !admission(c.nonempty, elapsed_ms, c.companion, c.accepted) {
                return;
            }
            let eligible = ELIGIBLE.fetch_add(1, Ordering::Relaxed) + 1;
            let slot = EMITTED.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < CAP).then_some(n + 1)
            });
            if slot.is_err() {
                let omitted = OMITTED.fetch_add(1, Ordering::Relaxed) + 1;
                if omitted == 1 {
                    println!(
                        "[PERF] CertifiedLayoutProbeCap cap={CAP} eligible_asof={eligible} omitted_asof={omitted} requests_asof={} lost_asof={} tail_coverage=unknown",
                        REQUESTS.load(Ordering::Relaxed),
                        LOST.load(Ordering::Relaxed)
                    );
                }
                return;
            }
            let m = c.marks;
            println!(
                "[PERF] CertifiedLayoutProbe ordinal={} thread={:?} pipeline={:?} epoch={:?} goal={:?} restyle={} pending={} nonempty_ordinal={} companion_on_entry={} admission_slow={} accepted_ordinal={:?} accepted_nodes={} utc_begin_us={} utc_end_us={} whole_ms={:.6} pre_prepare_ms={:?} overall_stage_guard_release_ms={:?} owner_tail_request_drop_ms={:?} entry_to_capture_ms={:?} capture_environment_ms={:?} input_prepare_ms={:?} guard_release_ms={:?} pre_ack_ms={:?} ack_ms={:?} ack_result={:?} commit_cleanup_ms={:?} accepted_tail_ms={:?} handle_request_drop_tail_ms={:?} attempt={:?} sample_offset={:?} sampling_enabled_on_attempt={} target_calls={} sampled_calls={} sampled_prepared={} sampled_rejected={} sample_proof_sum_ms={:.6} sample_build_sum_ms={:.6} sample_total_sum_ms={:.6} sample_kind=deterministic_rotating_1of64 sample_extrapolation=none emitted_asof={} eligible_asof={} omitted_asof={} requests_asof={} lost_asof={}",
                c.ordinal,
                std::thread::current().id(),
                c.pipeline,
                c.epoch,
                c.goal,
                c.has_restyle,
                c.pending,
                c.nonempty,
                c.companion,
                elapsed_ms >= 5.0,
                c.accepted,
                c.nodes,
                c.utc_start,
                utc_end,
                elapsed_ms,
                between(Some(c.start), m[1]),
                between(m[1], m[3]),
                between(m[6], Some(end)),
                between(Some(c.start), m[0]),
                between(m[0], m[1]),
                between(m[1], m[2]),
                between(m[2], m[3]),
                between(m[3], m[4]),
                between(m[4], m[5]),
                c.ack,
                between(m[5], m[6]),
                between(m[6], m[7]),
                between(m[7], Some(end)),
                c.attempt,
                c.attempt.map(|n| n % 64),
                c.sampling_enabled,
                c.samples.targets,
                c.samples.sampled,
                c.samples.prepared,
                c.samples.rejected,
                c.samples.proof_ms,
                c.samples.build_ms,
                c.samples.total_ms,
                slot.unwrap() + 1,
                eligible,
                OMITTED.load(Ordering::Relaxed),
                REQUESTS.load(Ordering::Relaxed),
                LOST.load(Ordering::Relaxed)
            );
        }
    }
    pub(super) struct Sample {
        context: Option<u64>,
        start: Option<Instant>,
        build: Option<Instant>,
        prepared: bool,
    }
    impl Sample {
        pub(super) fn begin() -> Self {
            let mut sample = Self {
                context: None,
                start: None,
                build: None,
                prepared: false,
            };
            if !enabled() {
                return sample;
            }
            CONTEXT.with(|stack| {
                let mut stack = stack.borrow_mut();
                let Some(c) = stack.last_mut() else {
                    return;
                };
                let Some(attempt) = c.attempt else {
                    return;
                };
                if !c.sampling_enabled {
                    return;
                }
                let index = c.samples.targets;
                c.samples.targets += 1;
                if sampled(index, attempt) {
                    sample.context = Some(c.ordinal);
                    sample.start = Some(Instant::now());
                }
            });
            sample
        }
        pub(super) fn build_started(&mut self) {
            if self.start.is_some() {
                self.build = Some(Instant::now());
            }
        }
        pub(super) fn prepared(&mut self) {
            self.prepared = true;
        }
    }
    impl Drop for Sample {
        fn drop(&mut self) {
            let Some(start) = self.start else {
                return;
            };
            let end = Instant::now();
            CONTEXT.with(|stack| {
                let mut stack = stack.borrow_mut();
                let Some(c) = stack
                    .iter_mut()
                    .rev()
                    .find(|c| Some(c.ordinal) == self.context)
                else {
                    LOST.fetch_add(1, Ordering::Relaxed);
                    return;
                };
                c.samples.sampled += 1;
                if self.prepared {
                    c.samples.prepared += 1;
                } else {
                    c.samples.rejected += 1;
                }
                c.samples.total_ms += between(Some(start), Some(end)).unwrap();
                c.samples.proof_ms +=
                    between(Some(start), Some(self.build.unwrap_or(end))).unwrap();
                c.samples.build_ms += between(self.build, Some(end)).unwrap_or(0.0);
            });
        }
    }
}

impl PreparedBackgroundStyle {
    /// Begin a gated diagnostic outer span; it has no style or frame authority.
    #[doc(hidden)]
    pub fn begin_layout_attribution(
        pipeline: &impl std::fmt::Debug,
        goal: &impl std::fmt::Debug,
        epoch: &impl std::fmt::Debug,
        has_restyle: bool,
        pending: usize,
    ) -> layout_attribution::Guard {
        layout_attribution::begin(pipeline, goal, epoch, has_restyle, pending)
    }
    /// Record a diagnostic boundary without retaining a TLS borrow.
    #[doc(hidden)]
    pub fn layout_attribution_mark(name: &'static str) {
        layout_attribution::mark(name);
    }
    /// Mark the actual paint reply, including a rejected reply.
    #[doc(hidden)]
    pub fn layout_attribution_ack(accepted: bool) {
        layout_attribution::ack(accepted);
    }
    /// Count actual accepted batches, independently of diagnostic row admission.
    #[doc(hidden)]
    pub fn layout_attribution_accepted(nodes: u32) {
        layout_attribution::accepted(nodes);
    }
    /// The first local in the restyle scope drops after its original locals.
    #[doc(hidden)]
    pub fn layout_attribution_exit() -> layout_attribution::Exit {
        layout_attribution::Exit
    }
}

#[cfg(test)]
mod layout_attribution_tests {
    use super::layout_attribution::{admission, sampled};
    #[test]
    fn certified_layout_probe_rotation_covers_both_parities_and_every_residue() {
        for index in 0..128 {
            assert_eq!(
                (1..=64).filter(|attempt| sampled(index, *attempt)).count(),
                1
            );
        }
        let offsets: Vec<_> = (1..=8)
            .map(|attempt| {
                (0..10000)
                    .filter(|index| sampled(*index, attempt))
                    .collect::<Vec<_>>()
            })
            .collect();
        assert!(
            offsets
                .iter()
                .all(|samples| samples.len() == 156 || samples.len() == 157)
        );
        assert!(offsets.iter().any(|samples| samples[0] % 2 == 0));
        assert!(offsets.iter().any(|samples| samples[0] % 2 == 1));
    }
    #[test]
    fn certified_layout_probe_admission_keeps_slow_initial_and_accepted_companions() {
        assert!(!admission(0, 0.01, false, None));
        assert!(admission(1, 0.01, false, None));
        assert!(admission(16, 0.01, false, None));
        assert!(!admission(17, 0.01, false, None));
        assert!(admission(0, 5.0, false, None));
        assert!(admission(0, 0.01, true, None));
        assert!(admission(0, 0.01, false, Some(8)));
        assert!(!admission(0, 0.01, false, Some(9)));
    }
}
