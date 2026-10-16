/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/. */

//! A deliberately narrow incremental recascade. Every missing proof falls back
//! to the ordinary cascade in the same traversal.

use crate::computed_value_flags::ComputedValueFlags as Flags;
use crate::context::{ElementCascadeInputs, StyleContext};
use crate::data::{EagerPseudoStyles, ElementData};
use crate::dom::{OpaqueNode, TElement, TNode};
use crate::invalidation::element::restyle_hints::RestyleHint;
use crate::properties::cascade::normal_inline_background_wins;
use crate::properties::declaration_block::{independent_background_color, Importance};
use crate::properties::{
    ComputedValues, LonghandId, PropertyDeclarationBlock, PropertyDeclarationId,
};
use crate::selector_parser::PseudoElement;
use crate::shared_lock::Locked;
use crate::style_resolver::{PrimaryStyle, ResolvedStyle};
use crate::values::computed::Color;
use crate::values::generics::image::Image;
use servo_arc::Arc;

mod batch;
pub use self::batch::{
    commit_background_style, prepare_background_style, prepare_background_style_in_batch,
    prepare_background_style_with_reuse, BackgroundBatchProof, BackgroundParentProof,
    BackgroundStyleReuse, PreparedBackgroundStyle,
};

/// One bounded successful baseline per element, never embedded in ComputedValues.
#[derive(Debug)]
pub(crate) struct InlineBackgroundBaseline {
    style: Arc<ComputedValues>,
    parent: OpaqueNode,
    parent_style: Arc<ComputedValues>,
    declarations: Arc<Locked<PropertyDeclarationBlock>>,
    revision: u64,
    epoch: u64,
    // Optional authority used only by the complete retained batch path.
    viewport: Option<BackgroundViewportStamp>,
    // Cleared by the consumer or the next restyle. No old-style history chain.
    previous: Option<Arc<ComputedValues>>,
    // A final, bounded UA-only Selection proof. No ComputedValues history chain.
    selection: Option<batch::SelectionBaseline>,
}

impl InlineBackgroundBaseline {
    pub(crate) fn take_witness(&mut self) -> Option<crate::data::BackgroundColorChangeWitness> {
        Some(crate::data::BackgroundColorChangeWitness {
            old: self.previous.take()?,
            new: self.style.clone(),
        })
    }
}

// Servo resolves every ViewportVariant from this one CSS viewport; screen size
// and DPR also participate in media features. Keep their exact positive finite
// bit patterns plus the independently invalidated Layout epoch, not a hash.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackgroundViewportStamp {
    epoch: u64,
    values: [u32; 5],
}

impl BackgroundViewportStamp {
    fn capture<E: TElement>(context: &StyleContext<E>) -> Option<Self> {
        Self::for_device(
            context.shared.inline_background_epoch?,
            context.shared.stylist.device(),
        )
    }

    fn for_device(epoch: u64, device: &crate::device::Device) -> Option<Self> {
        let css = device.viewport_size();
        let physical = device.device_size();
        Self::from_values(
            epoch,
            [
                css.width,
                css.height,
                physical.width,
                physical.height,
                device.device_pixel_ratio().get(),
            ],
        )
    }

    fn from_values(epoch: u64, values: [f32; 5]) -> Option<Self> {
        if values
            .iter()
            .any(|value| !value.is_finite() || *value <= 0.0)
        {
            return None;
        }
        Some(Self {
            epoch,
            values: values.map(f32::to_bits),
        })
    }
}

fn supported_style(style: &ComputedValues) -> bool {
    !style.is_pseudo_style() && supported_independent_inputs(style)
}

fn supported_independent_inputs(style: &ComputedValues) -> bool {
    let dependencies = Flags::IS_RELEVANT_LINK_VISITED
        | Flags::IS_IN_FIRST_LINE_SUBTREE
        | Flags::DISPLAY_OR_CONTENT_DEPEND_ON_INHERITED_STYLE
        | Flags::INHERITS_RESET_STYLE
        | Flags::DEPENDS_ON_SELF_FONT_METRICS
        | Flags::DEPENDS_ON_INHERITED_FONT_METRICS
        | Flags::IS_ROOT_ELEMENT_STYLE
        | Flags::USES_VIEWPORT_UNITS
        | Flags::USES_VIEWPORT_UNITS_ON_CONTAINER_QUERIES
        | Flags::SELF_OR_ANCESTOR_HAS_SIZE_CONTAINER_TYPE
        | Flags::USES_CONTAINER_UNITS
        | Flags::CONSIDERED_NONTRIVIAL_SCOPED_STYLE
        | Flags::DISPLAY_CONTENTS_IN_ITEM_CONTAINER
        | Flags::DEPENDS_ON_CONTAINER_STYLE_QUERY
        | Flags::USES_FONT_OR_WM_RELATIVE_UNITS
        | Flags::USES_FONT_OR_WM_RELATIVE_UNITS_ON_CONTAINER_QUERIES
        | Flags::tree_counting_function_flags();
    style.visited_style().is_none()
        && style.attribute_references.is_none()
        && !style.flags.intersects(dependencies)
        && !style.get_ui().specifies_animations()
        && !style.get_ui().specifies_transitions()
        && style
            .rules
            .as_ref()
            .is_some_and(|r| !r.has_animation_or_transition_rules())
        && !style
            .get_background()
            .background_image
            .0
            .iter()
            .any(|image| matches!(image, Image::PaintWorklet(_)))
}

// Only baseline recording and the separately stamped complete batch may use
// this alternative. The ordinary recascade and Selection guard above stay
// byte-for-byte unchanged. No container-query viewport flag is admitted.
fn supported_viewport_batch_inputs(style: &ComputedValues) -> bool {
    let dependencies = Flags::IS_RELEVANT_LINK_VISITED
        | Flags::IS_IN_FIRST_LINE_SUBTREE
        | Flags::DISPLAY_OR_CONTENT_DEPEND_ON_INHERITED_STYLE
        | Flags::INHERITS_RESET_STYLE
        | Flags::DEPENDS_ON_SELF_FONT_METRICS
        | Flags::DEPENDS_ON_INHERITED_FONT_METRICS
        | Flags::IS_ROOT_ELEMENT_STYLE
        | Flags::USES_VIEWPORT_UNITS_ON_CONTAINER_QUERIES
        | Flags::SELF_OR_ANCESTOR_HAS_SIZE_CONTAINER_TYPE
        | Flags::USES_CONTAINER_UNITS
        | Flags::CONSIDERED_NONTRIVIAL_SCOPED_STYLE
        | Flags::DISPLAY_CONTENTS_IN_ITEM_CONTAINER
        | Flags::DEPENDS_ON_CONTAINER_STYLE_QUERY
        | Flags::USES_FONT_OR_WM_RELATIVE_UNITS
        | Flags::USES_FONT_OR_WM_RELATIVE_UNITS_ON_CONTAINER_QUERIES
        | Flags::tree_counting_function_flags();
    style.visited_style().is_none()
        && style.attribute_references.is_none()
        && !style.flags.intersects(dependencies)
        && !style.get_ui().specifies_animations()
        && !style.get_ui().specifies_transitions()
        && style
            .rules
            .as_ref()
            .is_some_and(|r| !r.has_animation_or_transition_rules())
        && !style
            .get_background()
            .background_image
            .0
            .iter()
            .any(|image| matches!(image, Image::PaintWorklet(_)))
}

// Selection is eagerly stored even for empty elements by Servo's universal
// UA rule. Its style is still fully recascaded by the ordinary resolver under
// the new primary. Generated and other eager pseudos keep the full cascade.
fn supported_pseudos(pseudos: &EagerPseudoStyles) -> bool {
    pseudos.as_array().iter().enumerate().all(|(index, style)| {
        style.is_none() || PseudoElement::from_eager_index(index).is_selection()
    })
}

fn parent<E: TElement>(element: E) -> Option<(OpaqueNode, Arc<ComputedValues>)> {
    let parent = element.traversal_parent()?;
    let data = parent.borrow_data()?;
    let style = data.styles.primary.as_ref()?;
    // This makes the resolved layout parent identical to the traversal parent.
    if style.is_display_contents() {
        return None;
    }
    Some((parent.as_node().opaque(), style.clone()))
}

pub(crate) fn try_recascade<E: TElement>(
    context: &StyleContext<E>,
    element: E,
    data: &ElementData,
    inputs: &ElementCascadeInputs,
) -> Option<PrimaryStyle> {
    let epoch = context.shared.inline_background_epoch?;
    if data.hint.bits() != RestyleHint::RESTYLE_STYLE_ATTRIBUTE.bits()
        || !supported_pseudos(&data.styles.pseudos)
        || context.shared.traversal_flags.for_animation_only()
        || context.shared.stylist.device().forced_colors().is_active()
    {
        return None;
    }
    let baseline = data.inline_background.as_ref()?;
    let old = data.styles.primary.as_ref()?;
    if epoch != baseline.epoch || !Arc::ptr_eq(old, &baseline.style) || !supported_style(old) {
        return None;
    }
    let (parent, parent_style) = parent(element)?;
    // WAS_RESTYLED is intentionally not used: it can survive earlier passes.
    // Exact immutable parent identity covers the actual cascade inputs, including
    // a parent recascade that returns the same Arc; any child recascade/rematch
    // hint is separately rejected before restyle_kind can mask it.
    if parent != baseline.parent || !Arc::ptr_eq(&parent_style, &baseline.parent_style) {
        return None;
    }
    let declarations = element.style_attribute()?;
    if !std::ptr::eq(&*declarations, &*baseline.declarations) {
        return None;
    }
    let block = declarations.read_with(context.shared.guards.author);
    if !block.only_background_color_changes_since(baseline.revision) {
        return None;
    }
    let (declaration, importance) =
        block.get(PropertyDeclarationId::Longhand(LonghandId::BackgroundColor))?;
    if importance != Importance::Normal {
        return None;
    }
    let color = independent_background_color(declaration)?;
    let rules = inputs.primary.rules.as_ref()?;
    if inputs.primary.visited_rules.is_some()
        || inputs.primary.flags != old.flags.for_cascade_inputs()
        || !inputs.primary.included_cascade_flags.is_empty()
        || !normal_inline_background_wins(rules, &context.shared.guards, &declarations, color)
    {
        return None;
    }
    let mut values = (**old).clone();
    values.rules = Some(rules.clone());
    values.mutate_background().background_color = Color::Absolute(color);
    Some(PrimaryStyle {
        style: ResolvedStyle(Arc::new(values)),
        reused_via_rule_node: false,
    })
}

/// Called only after finish_restyle, including its normal animation processing.
pub(crate) fn record_final<E: TElement>(
    context: &mut StyleContext<E>,
    element: E,
    data: &mut ElementData,
    incremental: Option<(Arc<ComputedValues>, Arc<ComputedValues>)>,
) {
    if let Some((old, proposed)) = incremental {
        if data
            .styles
            .primary
            .as_ref()
            .is_some_and(|new| Arc::ptr_eq(new, &proposed))
        {
            // All inputs were proved by try_recascade. Holding the proposed Arc
            // across finish_restyle ensures any copy-on-write animation mutation
            // would change its identity and take the ordinary baseline path.
            if let Some(baseline) = data.inline_background.as_mut() {
                if let Some(revision) = baseline
                    .declarations
                    .read_with(context.shared.guards.author)
                    .background_color_revision()
                {
                    baseline.style = proposed;
                    baseline.revision = revision;
                    baseline.previous = Some(old);
                    baseline.selection = batch::selection_baseline(context, &data.styles.pseudos);
                    if context.shared.inline_background_count.is_some() {
                        context
                            .thread_local
                            .statistics
                            .inline_background_color_recascades += 1;
                    }
                    return;
                }
            }
        }
    }
    let create = || -> Option<InlineBackgroundBaseline> {
        let epoch = context.shared.inline_background_epoch?;
        let style = data.styles.primary.as_ref()?;
        let viewport = if style.flags.contains(Flags::USES_VIEWPORT_UNITS) {
            Some(BackgroundViewportStamp::capture(context)?)
        } else {
            None
        };
        // Recording is not admission: ordinary try_recascade still applies its
        // original strict guard. Only a later complete batch can match this stamp.
        let supported = supported_style(style)
            || (viewport.is_some()
                && !style.is_pseudo_style()
                && supported_viewport_batch_inputs(style));
        if !supported_pseudos(&data.styles.pseudos)
            || !supported
            || context.shared.traversal_flags.for_animation_only()
            || context.shared.stylist.device().forced_colors().is_active()
        {
            return None;
        }
        let (parent, parent_style) = parent(element)?;
        let declarations = element.style_attribute()?.clone_arc();
        let block = declarations.read_with(context.shared.guards.author);
        let revision = block.background_color_revision()?;
        let (declaration, importance) =
            block.get(PropertyDeclarationId::Longhand(LonghandId::BackgroundColor))?;
        if importance != Importance::Normal {
            return None;
        }
        let color = independent_background_color(declaration)?;
        if style.get_background().background_color != Color::Absolute(color)
            || !normal_inline_background_wins(
                style.rules.as_ref()?,
                &context.shared.guards,
                &declarations,
                color,
            )
        {
            return None;
        }
        Some(InlineBackgroundBaseline {
            style: style.clone(),
            parent,
            parent_style,
            declarations: declarations.clone(),
            revision,
            epoch,
            viewport,
            previous: None,
            selection: batch::selection_baseline(context, &data.styles.pseudos),
        })
    };
    let final_baseline = create();
    match (data.inline_background.as_mut(), final_baseline) {
        (Some(existing), Some(new)) => **existing = new,
        (None, Some(new)) => data.inline_background = Some(Box::new(new)),
        (_, None) => data.inline_background = None,
    }
}
