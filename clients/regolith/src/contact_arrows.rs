//! Screen-edge contact arrows: the first thing hearsay is allowed to draw (#610).
//!
//! # What one arrow says, exactly
//!
//! A chevron on the window edge plus a short ASCII line. Its whole assertion
//! is the `(seat, cell, age)` triple [`crate::hearsay`] received and nothing
//! else:
//!
//! * **bearing** — the screen direction from the player's own craft to the
//!   *centre* of the reported 512 m cell
//!   (`orrery_protocol::metres_from_cell_id` returns that cell's min corner;
//!   the centre is that plus half an edge on each axis). Both endpoints are
//!   stated facts: the player's own replicated position and the host's fold;
//!   re-aiming as the player moves is arithmetic over stated facts, not new
//!   knowledge;
//! * **age** — whole seconds, drawn as text, always. An arrow pointing at a
//!   five-to-ten-second-old cell is honest only when its staleness is
//!   legible, so the age is on the screen rather than in a tooltip, and it is
//!   also carried a second time in the chevron's alpha (fresh reads bright,
//!   near-expiry reads faint) because a glance is faster than a read;
//! * **who** — the roster's label when the roster has one. `None` means *no
//!   text at all* — never `UNKNOWN`, never `PLAYER 3` (`crate::roster`).
//!
//! # What it refuses to say
//!
//! * **No range.** No distance readout, no near/far band, no arrow scaled by
//!   separation. The datum locates a craft inside a 512 m cell as of 5-10 s
//!   ago; any number on screen would read as a measurement.
//! * **No motion.** The reported cell is frozen between folds. Nothing here
//!   interpolates, leads, trails, or fades a contact along a heading — no
//!   velocity was ever delivered, so none may be shown.
//! * **No contact-to-contact geometry.** Every arrow is drawn from one triple
//!   and the player's own position. That is why A16 chose arrows over a
//!   minimap, and why the de-cluttering below moves a crowded arrow *along
//!   its own bearing ray* rather than sideways along the edge: sliding it
//!   sideways would be the renderer inventing a bearing.
//! * **Nothing at all about a craft you can see.** While a live replica of a
//!   seat exists, that seat gets no arrow: the body on screen is strictly
//!   better knowledge, and a simultaneous arrow would be a second, staler
//!   assertion about the same subject.
//! * **Nothing about a fact older than the expiry horizon.** The state layer
//!   drops a seat the host has stopped naming; this layer independently
//!   refuses to draw any contact whose *stated age* has reached
//!   [`crate::hearsay::HEARSAY_EXPIRY_TICKS`]. The two guards key on
//!   different quantities — fold absence there, the fact's own age here — so
//!   neither one covers for the other.
//!
//! **Presentation only.** Like [`crate::aoi`], nothing in this module is
//! readable by intent submission, range, arc, lock or collision code: its
//! inputs are one render view plus a camera basis, and its only output is UI
//! nodes. It reads `ActiveSession` for the hearsay view exactly as
//! `sync_ship_labels` reads the roster for a name.
//!
//! # ASCII only
//!
//! No font asset is loaded, so Bevy's built-in ASCII-only face draws this
//! text; anything outside that subset renders as an empty box (#526). The
//! chevron is therefore geometry — two coloured borders on a rotated node —
//! rather than a glyph, which also keeps the bearing continuous instead of
//! quantised to the eight directions ASCII can spell.

use std::collections::{BTreeMap, BTreeSet};

use bevy::prelude::*;
use orrery_protocol::PersistId;

use crate::hearsay::{HearsayRenderContact, HearsayRenderView, HEARSAY_EXPIRY_TICKS};
use crate::net::HearsaySource;
use crate::roster::entity_of_slot;

/// Distance from the window edge to a chevron's centre, in pixels.
///
/// Far enough in that the chevron is whole rather than half-clipped, and that
/// its text has somewhere to sit; close enough that it still reads as "off
/// that way", not as an object in the world.
pub const ARROW_MARGIN_PX: f32 = 30.0;

/// The arrowhead's width, in pixels.
pub const ARROW_SIZE_PX: f32 = 14.0;

/// The arrowhead's height, in pixels.
///
/// Taller than it is wide so the mark reads as pointing rather than as a
/// generic marker at a glance.
pub const ARROW_HEIGHT_PX: f32 = 16.0;

/// Font size of the age (and, when known, name) line.
///
/// The same size as a ship's nickname tag: this is the same class of
/// annotation, and #552 records that 720 lines is already tight.
pub const ARROW_TEXT_PX: f32 = 12.0;

/// Gap between the arrowhead and its text, in pixels.
pub const ARROW_TEXT_GAP_PX: f32 = 5.0;

/// Padding inside an arrow's backing chip, in pixels.
///
/// The chip is why there is no second copy of the HUD's layout in this file.
/// An arrow lands wherever its bearing says, which at the bottom and top of a
/// 720-line window is on top of a HUD panel perhaps a seventh of the time.
/// Reserving a band for the panels would mean this module holding an opinion
/// about `hud::spawn_hud`'s content heights — the two-definitions mistake
/// #499 and #502 were, and the panels are content-sized, so the copy would be
/// wrong the first time a row is added. A chip of the HUD's own panel colour
/// makes the arrow read the same against the starfield, against a rock and
/// against a panel, and needs to know nothing about any of them.
pub const ARROW_CHIP_PAD_PX: f32 = 4.0;

/// Two chevrons closer together than this are treated as one cluster.
pub const ARROW_CLEAR_PX: f32 = 30.0;

/// How far along its own bearing ray a crowded arrow is pulled inward.
///
/// Inward along the ray, never sideways along the edge: the ray *is* the
/// assertion, and every point on it carries the same bearing.
pub const ARROW_STACK_PX: f32 = 22.0;

/// Alpha of a chevron whose fact is as fresh as the fold can deliver.
pub const ARROW_FRESH_ALPHA: f32 = 1.0;

/// Alpha of a chevron whose fact has aged to the expiry horizon.
///
/// Not zero: a contact one tick from expiry is still a contact, and a mark
/// that has faded to the background before it is dropped loses the player the
/// arrow earlier than the data does — the same argument `aoi::AOI_FADE_FLOOR`
/// makes for a fading hull.
pub const ARROW_STALE_ALPHA: f32 = 0.45;

/// One arrow the skin has decided to draw, in screen pixels.
#[derive(Debug, Clone, PartialEq)]
pub struct ArrowPlacement {
    /// The roster seat this arrow is about.
    pub seat: u8,
    /// Where the chevron's centre goes, in viewport pixels.
    pub at: Vec2,
    /// Clockwise rotation from screen-up that points the chevron down the
    /// bearing ray.
    pub rotation_rad: f32,
    /// The text drawn beside the chevron. Never empty: it always carries the
    /// age, and carries the roster label before it when one exists.
    pub text: String,
    /// The fact's age as a fraction of the expiry horizon: `1.0` at expiry.
    ///
    /// Normalised against the whole horizon rather than against the delivered
    /// `[F, 3F)` window, so the freshest arrow the host can send still reads as
    /// a third stale — which it is. The cost is a third of the alpha range
    /// left unused; the alternative would draw a five-second-old cell as if it
    /// were current.
    pub staleness: f32,
    /// Whether the text sits to the chevron's left (the arrow is on the right
    /// half of the screen) rather than to its right.
    pub text_on_left: bool,
}

/// Everything the placement decision is allowed to read.
///
/// Split out from [`sync_contact_arrows`] the way `ship_label_placements` is
/// split out of `sync_ship_labels`: the decisions that matter — suppression,
/// expiry, bearing, text — are asserted without a render device.
#[derive(Debug, Clone, Copy)]
pub struct ArrowScene<'a> {
    /// The hearsay view this frame.
    pub view: &'a HearsayRenderView,
    /// The player's own craft position, in the same grid-local metres the
    /// host's cells are derived from.
    pub own_m: Vec3,
    /// Where the player's own craft projects to on screen.
    pub own_screen: Vec2,
    /// The camera's world-space right and up axes.
    pub camera_right: Vec3,
    /// The camera's world-space up axis.
    pub camera_up: Vec3,
    /// The viewport, in pixels.
    pub viewport: Vec2,
    /// The campaign's interest cell edge, in metres.
    pub cell_edge_m: f32,
    /// Every core entity this client currently has a drawn body for.
    pub live_bodies: &'a BTreeSet<PersistId>,
}

/// The age line for one contact: `"NAME 7s"`, or `"7s"` when unnamed.
///
/// Whole seconds, floored: the fact is a cell, and a decimal on its age would
/// dress a 5-to-10-second staleness window as a measurement.
#[must_use]
pub fn arrow_text(label: Option<&str>, age_ticks: u64, source: HearsaySource) -> String {
    let seconds = age_ticks / u64::from(orrery_core::TICK_HZ);
    let age = format!("{seconds}s");
    match (label, source_tag(source)) {
        (Some(name), None) => format!("{name} {age}"),
        (Some(name), Some(tag)) => format!("{name} {age} {tag}"),
        (None, None) => age,
        (None, Some(tag)) => format!("{age} {tag}"),
    }
}

/// The visible provenance tag for a source, or `None` when it needs none.
///
/// H3 wants hearsay source-labelled end to end, and the record does carry the
/// source. Today exactly one source exists — the campaign host's own roster
/// fold, the same party every replicated byte on screen already comes from —
/// so printing `HOST` beside every arrow would spend scarce 720-line pixels
/// restating what the whole HUD already is. The match is deliberately
/// exhaustive rather than a `_` arm: the day a second source exists this stops
/// compiling, and whoever adds it has to decide what the player is told.
const fn source_tag(source: HearsaySource) -> Option<&'static str> {
    match source {
        HearsaySource::HostRosterFold => None,
    }
}

/// Which arrows to draw this frame, and where.
///
/// The three refusals are here, each keyed on its own quantity:
/// a seat with a live body is skipped, a fact at or past the expiry horizon is
/// skipped, and a contact whose bearing does not resolve to a direction is
/// skipped.
#[must_use]
pub fn contact_arrow_placements(scene: &ArrowScene<'_>) -> Vec<ArrowPlacement> {
    let mut placed: Vec<ArrowPlacement> = Vec::new();
    for contact in scene.view.contacts() {
        // If you can see the ship, you do not get an arrow for it. The drawn
        // body is fresher and finer than any cell fix, and two marks for one
        // craft is the skin asserting twice about one subject.
        if scene
            .live_bodies
            .contains(&entity_of_slot(usize::from(contact.seat)))
        {
            continue;
        }
        // A fact that has aged past the horizon is not drawn even if the host
        // is still naming its seat: the arrow's age label would be outside the
        // window this product promises.
        if contact.age_ticks >= HEARSAY_EXPIRY_TICKS {
            continue;
        }
        let Some(direction) = bearing_on_screen(scene, contact) else {
            continue;
        };
        let mut at = edge_point(scene.own_screen, direction, scene.viewport);
        // Crowding is resolved along the ray, so the bearing survives it.
        while placed
            .iter()
            .any(|other| other.at.distance(at) < ARROW_CLEAR_PX)
        {
            let pulled = at - direction * ARROW_STACK_PX;
            if pulled.distance(scene.own_screen) < ARROW_STACK_PX {
                // Refuse to walk an arrow all the way onto the player's own
                // craft; overlapping marks are better than a mark in the
                // middle of the screen, which would read as a world object.
                break;
            }
            at = pulled;
        }
        placed.push(ArrowPlacement {
            seat: contact.seat,
            at,
            rotation_rad: direction.x.atan2(-direction.y),
            text: arrow_text(contact.label.as_deref(), contact.age_ticks, contact.source),
            staleness: (contact.age_ticks as f32 / HEARSAY_EXPIRY_TICKS as f32).clamp(0.0, 1.0),
            text_on_left: at.x > scene.viewport.x * 0.5,
        });
    }
    placed
}

/// The unit screen direction from the player to a contact's cell centre.
///
/// `None` when the two coincide closely enough that no direction exists —
/// which is the honest answer, since a contact inside the player's own cell
/// has no bearing the datum supports.
fn bearing_on_screen(scene: &ArrowScene<'_>, contact: &HearsayRenderContact) -> Option<Vec2> {
    let corner = orrery_protocol::metres_from_cell_id(contact.cell, f64::from(scene.cell_edge_m));
    let half = f64::from(scene.cell_edge_m) / 2.0;
    #[allow(clippy::cast_possible_truncation)]
    let centre = Vec3::new(
        (corner.x + half) as f32,
        (corner.y + half) as f32,
        (corner.z + half) as f32,
    );
    let delta = centre - scene.own_m;
    // Screen y grows downward, so the camera's up axis enters negated.
    let screen = Vec2::new(delta.dot(scene.camera_right), -delta.dot(scene.camera_up));
    screen.try_normalize()
}

/// Where a ray from `origin` leaves the viewport rectangle, inset by
/// [`ARROW_MARGIN_PX`].
///
/// The rectangle rather than an inscribed circle: a ring wastes the corners,
/// and at 1280x720 the corners are where a quarter of the bearings point.
#[must_use]
pub fn edge_point(origin: Vec2, direction: Vec2, viewport: Vec2) -> Vec2 {
    let margin = ARROW_MARGIN_PX.min(viewport.x * 0.5).min(viewport.y * 0.5);
    let min = Vec2::splat(margin);
    let max = (viewport - Vec2::splat(margin)).max(min);
    let start = origin.clamp(min, max);
    let axis = |from: f32, dir: f32, low: f32, high: f32| {
        if dir.abs() < f32::EPSILON {
            f32::INFINITY
        } else if dir > 0.0 {
            (high - from) / dir
        } else {
            (low - from) / dir
        }
    };
    let travel = axis(start.x, direction.x, min.x, max.x)
        .min(axis(start.y, direction.y, min.y, max.y))
        .max(0.0);
    if travel.is_finite() {
        (start + direction * travel).clamp(min, max)
    } else {
        start
    }
}

/// The chevron's colour at a given staleness.
#[must_use]
pub fn arrow_colour(staleness: f32) -> Color {
    let alpha =
        ARROW_FRESH_ALPHA + (ARROW_STALE_ALPHA - ARROW_FRESH_ALPHA) * staleness.clamp(0.0, 1.0);
    crate::hud::ACCENT_PALE.with_alpha(alpha)
}

/// The text's colour at a given staleness.
#[must_use]
pub fn arrow_text_colour(staleness: f32) -> Color {
    let alpha =
        ARROW_FRESH_ALPHA + (ARROW_STALE_ALPHA - ARROW_FRESH_ALPHA) * staleness.clamp(0.0, 1.0);
    crate::hud::MUTED.with_alpha(alpha)
}

/// Height of one arrow cluster: the taller of the chevron and its text line.
#[must_use]
pub fn cluster_height_px() -> f32 {
    2.0 * ARROW_CHIP_PAD_PX + ARROW_HEIGHT_PX.max(ARROW_TEXT_PX * crate::legend::LINE_HEIGHT_RATIO)
}

/// Estimated width of one arrow's whole cluster, in pixels.
///
/// Used to centre the cluster on its placement point before Bevy has laid it
/// out, the same one-frame-early estimate `sync_ship_labels` makes.
#[must_use]
pub fn cluster_width_px(text: &str) -> f32 {
    2.0 * ARROW_CHIP_PAD_PX
        + ARROW_SIZE_PX
        + ARROW_TEXT_GAP_PX
        + crate::legend::text_width_px(text, ARROW_TEXT_PX)
}

/// Marks one drawn arrow cluster and the seat it speaks for.
#[derive(Component, Debug, Clone, Copy)]
pub struct ContactArrow(
    /// The roster seat.
    pub u8,
);

/// Marks the rotated arrowhead inside a cluster.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct ContactArrowHead;

/// Marks the age line inside a cluster.
#[derive(Component, Debug, Clone, Copy)]
pub(crate) struct ContactArrowText;

/// Draws one edge arrow per eligible hearsay contact.
#[allow(clippy::too_many_arguments)]
pub fn sync_contact_arrows(
    session: Res<crate::ActiveSession>,
    roster: Res<crate::roster::ShipRoster>,
    camera: Query<(&Camera, &GlobalTransform), With<crate::ChaseCamera>>,
    bodies: Query<(&crate::CoreEntity, &GlobalTransform), With<crate::CraftBodyComposition>>,
    windows: Query<&Window, With<bevy::window::PrimaryWindow>>,
    clusters: Query<(Entity, &ContactArrow, &Children)>,
    mut nodes: Query<&mut Node>,
    mut transforms: Query<&mut UiTransform, With<ContactArrowHead>>,
    mut borders: Query<&mut BorderColor, With<ContactArrowHead>>,
    mut texts: Query<(&mut Text, &mut TextColor), With<ContactArrowText>>,
    mut commands: Commands,
) {
    let placements = {
        let Some(view) = session.hearsay_view(&roster) else {
            despawn_all(&clusters, &mut commands);
            return;
        };
        let (Ok((camera, camera_transform)), Ok(window)) = (camera.single(), windows.single())
        else {
            return;
        };
        let Some(cell_edge_m) = session.aoi_edge_m() else {
            despawn_all(&clusters, &mut commands);
            return;
        };
        let own = session.local_entity();
        let live: BTreeSet<PersistId> = bodies.iter().map(|(core, _)| core.0).collect();
        let Some(own_m) = bodies
            .iter()
            .find_map(|(core, at)| (core.0 == own).then(|| at.translation()))
        else {
            // No own body yet: there is no point to take a bearing from, and
            // guessing one would be the skin inventing the player's position.
            despawn_all(&clusters, &mut commands);
            return;
        };
        let viewport = Vec2::new(window.width(), window.height());
        let own_screen = camera
            .world_to_viewport(camera_transform, own_m)
            .unwrap_or(viewport * 0.5);
        contact_arrow_placements(&ArrowScene {
            view: &view,
            own_m,
            own_screen,
            camera_right: camera_transform.right().into(),
            camera_up: camera_transform.up().into(),
            viewport,
            cell_edge_m,
            live_bodies: &live,
        })
    };

    let wanted: BTreeMap<u8, ArrowPlacement> = placements
        .into_iter()
        .map(|placement| (placement.seat, placement))
        .collect();
    let mut drawn = BTreeSet::new();
    for (cluster, arrow, children) in &clusters {
        let Some(placement) = wanted.get(&arrow.0) else {
            commands.entity(cluster).despawn();
            continue;
        };
        drawn.insert(arrow.0);
        if let Ok(mut node) = nodes.get_mut(cluster) {
            apply_cluster_node(&mut node, placement);
        }
        for child in children {
            if let Ok(mut transform) = transforms.get_mut(*child) {
                transform.rotation = head_rotation(placement.rotation_rad);
            }
            if let Ok(mut border) = borders.get_mut(*child) {
                *border = head_border(placement.staleness);
            }
            if let Ok((mut text, mut colour)) = texts.get_mut(*child) {
                if **text != placement.text {
                    **text = placement.text.clone();
                }
                *colour = TextColor(arrow_text_colour(placement.staleness));
            }
        }
    }
    for (seat, placement) in wanted {
        if drawn.contains(&seat) {
            continue;
        }
        let mut node = cluster_node();
        apply_cluster_node(&mut node, &placement);
        commands
            .spawn((
                ContactArrow(seat),
                node,
                BackgroundColor(crate::hud::PANEL),
                GlobalZIndex(60),
            ))
            .with_children(|cluster| {
                cluster.spawn((
                    ContactArrowHead,
                    head_node(),
                    head_border(placement.staleness),
                    UiTransform::from_rotation(head_rotation(placement.rotation_rad)),
                ));
                cluster.spawn((
                    ContactArrowText,
                    Text::new(placement.text.clone()),
                    TextFont::from_font_size(ARROW_TEXT_PX),
                    TextColor(arrow_text_colour(placement.staleness)),
                ));
            });
    }
}

fn despawn_all(clusters: &Query<(Entity, &ContactArrow, &Children)>, commands: &mut Commands) {
    for (cluster, _, _) in clusters {
        commands.entity(cluster).despawn();
    }
}

/// The cluster's own layout: a row holding the chevron and its text.
fn cluster_node() -> Node {
    Node {
        position_type: PositionType::Absolute,
        align_items: AlignItems::Center,
        column_gap: Val::Px(ARROW_TEXT_GAP_PX),
        padding: UiRect::all(Val::Px(ARROW_CHIP_PAD_PX)),
        border_radius: BorderRadius::all(Val::Px(4.0)),
        ..Default::default()
    }
}

/// Places a cluster so its chevron sits on the placement point and its text
/// falls toward the middle of the screen rather than off the edge.
fn apply_cluster_node(node: &mut Node, placement: &ArrowPlacement) {
    node.flex_direction = if placement.text_on_left {
        FlexDirection::RowReverse
    } else {
        FlexDirection::Row
    };
    let width = cluster_width_px(&placement.text);
    let head_edge = ARROW_CHIP_PAD_PX + ARROW_SIZE_PX * 0.5;
    let left = if placement.text_on_left {
        placement.at.x + head_edge - width
    } else {
        placement.at.x - head_edge
    };
    node.left = Val::Px(left);
    node.top = Val::Px(placement.at.y - cluster_height_px() * 0.5);
}

/// The arrowhead: a zero-content box whose side borders are transparent and
/// whose bottom border is not, which mitres into a filled triangle pointing
/// up. A solid mark rather than two strokes because a thin open chevron reads
/// as a tick at 12-14 px, and because a filled shape keeps its identity
/// against both the starfield and a lit rock.
fn head_node() -> Node {
    Node {
        width: Val::Px(0.0),
        height: Val::Px(0.0),
        border: UiRect {
            left: Val::Px(ARROW_SIZE_PX / 2.0),
            right: Val::Px(ARROW_SIZE_PX / 2.0),
            bottom: Val::Px(ARROW_HEIGHT_PX),
            top: Val::Px(0.0),
        },
        ..Default::default()
    }
}

/// The arrowhead's rotation. The triangle is authored pointing up, so the
/// bearing — measured clockwise from screen-up — is the whole rotation.
fn head_rotation(bearing_rad: f32) -> Rot2 {
    Rot2::radians(bearing_rad)
}

fn head_border(staleness: f32) -> BorderColor {
    BorderColor {
        top: Color::NONE,
        left: Color::NONE,
        right: Color::NONE,
        bottom: arrow_colour(staleness),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hearsay::{HearsayState, HEARSAY_FOLD_TICKS};
    use crate::net::{HearsayContact, HearsayContacts};
    use crate::roster::{RosterResponse, RosterRow, ShipRoster};
    use orrery_protocol::{CellId, INTEREST_LEVEL};

    const EDGE_M: f32 = 512.0;
    const VIEWPORT: Vec2 = Vec2::new(1280.0, 720.0);

    /// A cell whose centre lies `cells` east of the origin cell's centre.
    fn cell_at(x: i32, z: i32) -> CellId {
        CellId::from_coords(IVec3::new(x, 0, z), INTEREST_LEVEL).expect("cell in range")
    }

    fn view_with(contacts: Vec<(u8, CellId, u64, Option<&str>)>) -> HearsayRenderView {
        let mut state = HearsayState::default();
        let mut roster = ShipRoster::default();
        let mut rows = Vec::new();
        for (seat, _, _, label) in &contacts {
            if let Some(name) = label {
                rows.push(RosterRow::labelled(usize::from(*seat), name));
            }
        }
        roster.accept(
            &RosterResponse {
                roster: rows,
                ..RosterResponse::default()
            },
            None,
        );
        // Every contact is delivered by one fold; the per-contact age is
        // carried as the fact age the host stamped.
        state.accept(
            HearsayContacts {
                source: HearsaySource::HostRosterFold,
                fold_tick: 10_000,
                contacts: contacts
                    .iter()
                    .map(|(seat, cell, age_ticks, _)| HearsayContact {
                        seat: *seat,
                        cell: cell.to_bits(),
                        fact_age_ticks: u16::try_from(*age_ticks).expect("test age fits"),
                    })
                    .collect(),
            },
            10_000,
        );
        state.render_view(&roster, 10_000)
    }

    fn scene<'a>(view: &'a HearsayRenderView, live: &'a BTreeSet<PersistId>) -> ArrowScene<'a> {
        ArrowScene {
            view,
            // The player sits in the middle of the origin cell.
            own_m: Vec3::new(EDGE_M / 2.0, 0.0, EDGE_M / 2.0),
            own_screen: VIEWPORT * 0.5,
            // The chase camera looks straight down with -Z up the screen, so
            // world +X runs right and world +Z runs down.
            camera_right: Vec3::X,
            camera_up: Vec3::NEG_Z,
            viewport: VIEWPORT,
            cell_edge_m: EDGE_M,
            live_bodies: live,
        }
    }

    #[test]
    fn an_arrow_points_down_the_bearing_to_the_reported_cell() {
        let view = view_with(vec![(3, cell_at(6, 0), 300, None)]);
        let live = BTreeSet::new();
        let drawn = contact_arrow_placements(&scene(&view, &live));
        let arrow = drawn.first().expect("a contact due east draws an arrow");
        assert!(
            (arrow.at.y - VIEWPORT.y / 2.0).abs() < 0.5,
            "a due-east contact sits on the horizontal midline, got {:?}",
            arrow.at
        );
        assert!(
            arrow.at.x > VIEWPORT.x - ARROW_MARGIN_PX - 0.5,
            "a due-east contact sits on the right edge, got {:?}",
            arrow.at
        );
        // Clockwise from screen-up: due east is a quarter turn.
        assert!(
            (arrow.rotation_rad - std::f32::consts::FRAC_PI_2).abs() < 1e-3,
            "east is a quarter turn clockwise from up, got {}",
            arrow.rotation_rad
        );

        let north = view_with(vec![(3, cell_at(0, -6), 300, None)]);
        let drawn = contact_arrow_placements(&scene(&north, &live));
        let arrow = drawn.first().expect("a contact due north draws an arrow");
        assert!(
            arrow.at.y < ARROW_MARGIN_PX + 0.5 && (arrow.at.x - VIEWPORT.x / 2.0).abs() < 0.5,
            "a due-north contact sits on the top edge, got {:?}",
            arrow.at
        );
        assert!(
            arrow.rotation_rad.abs() < 1e-3,
            "north is no rotation from up, got {}",
            arrow.rotation_rad
        );
    }

    /// H3: an arrow that does not say how old it is asserts a freshness the
    /// fold never delivered, so text and arrow stand or fall together.
    #[test]
    fn every_arrow_carries_ascii_age_text_and_a_label_only_when_one_exists() {
        let view = view_with(vec![
            (2, cell_at(6, 0), 300, Some("ORCA")),
            (5, cell_at(0, 6), 420, None),
        ]);
        let live = BTreeSet::new();
        let drawn = contact_arrow_placements(&scene(&view, &live));
        assert_eq!(drawn.len(), 2, "two eligible contacts draw two arrows");
        for arrow in &drawn {
            assert!(
                !arrow.text.is_empty(),
                "seat {} drew an arrow with no age text",
                arrow.seat
            );
            assert!(
                arrow.text.is_ascii(),
                "seat {} drew non-ASCII text {:?}, which Bevy's face renders as boxes",
                arrow.seat,
                arrow.text
            );
            assert!(
                arrow.text.ends_with('s') && arrow.text.chars().any(|c| c.is_ascii_digit()),
                "seat {} drew {:?}, which states no age in seconds",
                arrow.seat,
                arrow.text
            );
        }
        let named = drawn.iter().find(|a| a.seat == 2).expect("seat 2 drawn");
        assert_eq!(named.text, "ORCA 5s", "a known label leads the age");
        let unnamed = drawn.iter().find(|a| a.seat == 5).expect("seat 5 drawn");
        assert_eq!(
            unnamed.text, "7s",
            "an unknown seat gets the age alone, never UNKNOWN or a seat number"
        );
    }

    #[test]
    fn no_arrow_while_a_replica_of_that_seat_is_live() {
        let view = view_with(vec![(2, cell_at(6, 0), 300, Some("ORCA"))]);
        let live: BTreeSet<PersistId> = [entity_of_slot(2)].into_iter().collect();
        let drawn = contact_arrow_placements(&scene(&view, &live));
        assert!(
            drawn.is_empty(),
            "seat 2 has a drawn body, so the hearsay arrow must be suppressed; got {drawn:?}"
        );

        let elsewhere: BTreeSet<PersistId> = [entity_of_slot(4)].into_iter().collect();
        let drawn = contact_arrow_placements(&scene(&view, &elsewhere));
        assert_eq!(
            drawn.len(),
            1,
            "another seat's live body must not suppress seat 2's arrow"
        );
    }

    /// The renderer's own guard, on the fact's stated age rather than on the
    /// host having stopped naming the seat. Both quantities have to be able
    /// to remove an arrow on their own.
    #[test]
    fn no_arrow_once_the_stated_age_reaches_the_expiry_horizon() {
        let live = BTreeSet::new();
        let fresh = view_with(vec![(2, cell_at(6, 0), HEARSAY_EXPIRY_TICKS - 1, None)]);
        assert_eq!(
            fresh.contacts()[0].age_ticks,
            HEARSAY_EXPIRY_TICKS - 1,
            "the view carries the age this test means to exercise"
        );
        assert_eq!(
            contact_arrow_placements(&scene(&fresh, &live)).len(),
            1,
            "a contact one tick short of the horizon is still drawable"
        );

        let expired = view_with(vec![(2, cell_at(6, 0), HEARSAY_EXPIRY_TICKS, None)]);
        assert_eq!(
            expired.contacts()[0].age_ticks,
            HEARSAY_EXPIRY_TICKS,
            "the view carries the expired age this test means to exercise"
        );
        let drawn = contact_arrow_placements(&scene(&expired, &live));
        assert!(
            drawn.is_empty(),
            "a fact aged {HEARSAY_EXPIRY_TICKS} ticks ({} s) is past the {} s horizon this \
             product promises and must not be drawn; got {drawn:?}",
            HEARSAY_EXPIRY_TICKS / u64::from(orrery_core::TICK_HZ),
            HEARSAY_EXPIRY_TICKS / u64::from(orrery_core::TICK_HZ),
        );
    }

    /// The state layer expires on fold absence; this pins that an arrow really
    /// does disappear when it does, rather than the renderer holding its own
    /// copy of the last view.
    #[test]
    fn no_arrow_once_the_hearsay_state_has_expired_the_seat() {
        let mut state = HearsayState::default();
        let roster = ShipRoster::default();
        state.accept(
            HearsayContacts {
                source: HearsaySource::HostRosterFold,
                fold_tick: 10_000,
                contacts: vec![HearsayContact {
                    seat: 2,
                    cell: cell_at(6, 0).to_bits(),
                    fact_age_ticks: 300,
                }],
            },
            10_000,
        );
        let live = BTreeSet::new();
        let view = state.render_view(&roster, 10_000);
        assert_eq!(contact_arrow_placements(&scene(&view, &live)).len(), 1);

        state.expire(10_000 + HEARSAY_EXPIRY_TICKS);
        let view = state.render_view(&roster, 10_000 + HEARSAY_EXPIRY_TICKS);
        assert!(
            contact_arrow_placements(&scene(&view, &live)).is_empty(),
            "a seat the host stopped naming three folds ago has no arrow"
        );
    }

    /// Clutter, resolved without lying: two contacts on the same bearing keep
    /// that bearing and stack along its ray.
    #[test]
    fn contacts_sharing_a_bearing_stack_along_the_ray_not_along_the_edge() {
        let view = view_with(vec![
            (2, cell_at(6, 0), 300, Some("ORCA")),
            (5, cell_at(9, 0), 300, Some("PIKE")),
        ]);
        let live = BTreeSet::new();
        let drawn = contact_arrow_placements(&scene(&view, &live));
        assert_eq!(drawn.len(), 2);
        let (first, second) = (&drawn[0], &drawn[1]);
        assert!(
            (first.rotation_rad - second.rotation_rad).abs() < 1e-3,
            "both contacts are due east, so both arrows must still point east"
        );
        assert!(
            first.at.distance(second.at) >= ARROW_CLEAR_PX - 0.001,
            "the two arrows overlap at {:?} and {:?}",
            first.at,
            second.at
        );
        assert!(
            (first.at.y - second.at.y).abs() < 0.5,
            "stacking must stay on the shared bearing ray, not slide along the edge: {:?} vs {:?}",
            first.at,
            second.at
        );
        assert!(
            second.at.x < first.at.x,
            "the crowded arrow is pulled inward along its own ray"
        );
    }

    /// Every bearing lands inside the window, including the diagonals a ring
    /// would have pushed into the corners.
    #[test]
    fn every_bearing_lands_inside_the_default_720_line_window() {
        for degrees in 0..360 {
            let radians = (degrees as f32).to_radians();
            let direction = Vec2::new(radians.cos(), radians.sin());
            let at = edge_point(VIEWPORT * 0.5, direction, VIEWPORT);
            assert!(
                at.x >= ARROW_MARGIN_PX - 0.01
                    && at.x <= VIEWPORT.x - ARROW_MARGIN_PX + 0.01
                    && at.y >= ARROW_MARGIN_PX - 0.01
                    && at.y <= VIEWPORT.y - ARROW_MARGIN_PX + 0.01,
                "bearing {degrees} deg placed an arrow at {at:?}, outside the inset rectangle"
            );
            let on_edge = (at.x - ARROW_MARGIN_PX).abs() < 0.01
                || (at.x - (VIEWPORT.x - ARROW_MARGIN_PX)).abs() < 0.01
                || (at.y - ARROW_MARGIN_PX).abs() < 0.01
                || (at.y - (VIEWPORT.y - ARROW_MARGIN_PX)).abs() < 0.01;
            assert!(
                on_edge,
                "bearing {degrees} deg did not reach an edge: {at:?}"
            );
        }
    }

    /// The fit proof at the default window Bevy opens, 1280x720 (#552).
    #[test]
    fn an_arrow_cluster_fits_the_default_720_line_window() {
        // The longest line the skin can produce: the roster caps a nickname,
        // and the age is bounded by the expiry horizon.
        let longest_age = HEARSAY_EXPIRY_TICKS / u64::from(orrery_core::TICK_HZ);
        let name = "M".repeat(crate::roster::NICKNAME_MAX_CHARS);
        let text = arrow_text(
            Some(&name),
            longest_age * u64::from(orrery_core::TICK_HZ),
            HearsaySource::HostRosterFold,
        );
        let width = cluster_width_px(&text);
        assert!(
            width <= VIEWPORT.x / 2.0 - ARROW_MARGIN_PX,
            "the widest arrow cluster measures {width:.1} px and must fit between the edge \
             and the centre of a 1280 px window"
        );
        const {
            assert!(
                ARROW_TEXT_PX * crate::legend::LINE_HEIGHT_RATIO + ARROW_SIZE_PX
                    <= ARROW_MARGIN_PX * 2.0,
                "an arrow cluster is taller than the margin reserved for it"
            );
        }
    }

    /// A fresh fact must read differently from one about to expire, without
    /// either becoming invisible.
    #[test]
    fn staleness_dims_an_arrow_without_hiding_it() {
        let fresh = view_with(vec![(2, cell_at(6, 0), HEARSAY_FOLD_TICKS, None)]);
        let live = BTreeSet::new();
        let fresh = contact_arrow_placements(&scene(&fresh, &live));
        let old = view_with(vec![(2, cell_at(6, 0), HEARSAY_EXPIRY_TICKS - 60, None)]);
        let old = contact_arrow_placements(&scene(&old, &live));
        assert!(fresh[0].staleness < old[0].staleness);
        assert!(
            arrow_colour(old[0].staleness).alpha() >= ARROW_STALE_ALPHA,
            "an arrow one second from expiry must still be visible"
        );
        assert!(
            arrow_colour(fresh[0].staleness).alpha() > arrow_colour(old[0].staleness).alpha(),
            "staleness must be legible at a glance as well as in the text"
        );
    }
}
