//! The controls legend: what a player who has never flown this can press.
//!
//! Rotate-and-thrust with momentum is not guessable from a blank screen, and
//! #564 records what a first-time player is told today: nothing. This module
//! is the answer — a dimmed panel in the free bottom-right corner naming every
//! input the skin reads, which retires itself once the player has demonstrated
//! that they know it, and comes back on `F1`.
//!
//! **Presentation only, and unusually strictly so.** The legend describes
//! *inputs*. It reads no executor state, no `CombatView`, no roster; it shows
//! no number the ruleset produced and cannot therefore assert anything the
//! ruleset has not (A12 section 5.6, ADR-0050). The only state it keeps is
//! which keys this player has pressed, which is a fact about the keyboard.
//!
//! **ASCII only.** Bevy's default font renders anything outside its subset as
//! an empty box, so there are no arrow glyphs here: the rows say `Left`,
//! `Right`, `Up` and `Space` in words. `legend_text_is_ascii_only` pins it.

use bevy::prelude::*;

use crate::hud::{ACCENT_PALE, DIM, FAINT, MUTED, PANEL};

/// One input the skin actually reads, as the legend names it.
///
/// The variants are the *bindings*, not the rows: a test presses the key each
/// row names and asserts the binding it claims really fires, so an input that
/// moves in `crate::controls` and not here fails a named test.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Bound {
    /// Turn the craft, either way.
    Yaw,
    /// Apply the pilot's acceleration.
    Thrust,
    /// Emit one trigger order.
    Fire,
    /// Pick the locked target with the mouse.
    Select,
    /// Move the chase camera in and out.
    Zoom,
    /// Toggle the F3 correctness overlay.
    Overlay,
    /// Toggle this legend.
    Legend,
}

/// Every binding a row must exist for.
pub const BOUND: [Bound; 7] = [
    Bound::Yaw,
    Bound::Thrust,
    Bound::Fire,
    Bound::Select,
    Bound::Zoom,
    Bound::Overlay,
    Bound::Legend,
];

/// The bindings whose use retires the legend on its own.
///
/// `Overlay` and `Legend` are deliberately absent: they are the diagnostic and
/// the legend's own switch, and a player who never opens either has still
/// learned to fly. Waiting on them would leave the panel up for a session.
pub const FLIGHT: [Bound; 5] = [
    Bound::Yaw,
    Bound::Thrust,
    Bound::Fire,
    Bound::Select,
    Bound::Zoom,
];

/// One line of the legend: what to press, and what pressing it does.
#[derive(Debug, Clone, Copy)]
pub struct Row {
    /// Which binding this row describes.
    pub bound: Bound,
    /// The input, spelled the way a player would say it out loud.
    pub keys: &'static str,
    /// What that input does. An input, never an adjudicated result.
    pub action: &'static str,
}

/// The legend, in the order it is drawn.
///
/// Flight first, because a player who cannot move has no use for the rest.
pub const ROWS: [Row; 7] = [
    Row {
        bound: Bound::Yaw,
        keys: "Left / Right",
        action: "turn",
    },
    Row {
        bound: Bound::Thrust,
        keys: "Up",
        action: "thrust - you keep drifting",
    },
    Row {
        bound: Bound::Fire,
        keys: "Space",
        action: "fire",
    },
    Row {
        bound: Bound::Select,
        keys: "Click a ship",
        action: "pick it as your target",
    },
    Row {
        bound: Bound::Zoom,
        keys: "Mouse wheel",
        action: "zoom",
    },
    Row {
        bound: Bound::Overlay,
        keys: "F3",
        action: "correctness overlay",
    },
    Row {
        bound: Bound::Legend,
        keys: "F1",
        action: "show or hide this list",
    },
];

/// The panel's heading.
pub const HEADING: &str = "CONTROLS";

/// How to mine, which is otherwise undiscoverable.
///
/// There is no mining key. A rock is a lockable body like a ship, so mining is
/// the ordinary select-then-fire loop pointed at a rock — and nothing on the
/// screen says so.
pub const NOTE: &str = "To mine: click a rock, then hold Space.";

/// How to collect a pickup, which has no key at all.
///
/// #568's answer is proximity, not a binding: the skin emits the grab when the
/// craft is inside the ruleset's own reach. So this is a *statement* of the
/// mechanism and not a row — there is no key to name. It is
/// [`crate::grab::PICKUP_STATEMENT`], the same string the own-craft panel
/// shows, so the two cannot say different things.
pub const PICKUP_NOTE: &str = crate::grab::PICKUP_STATEMENT;

/// Font size of a legend row, in pixels.
pub const ROW_FONT_PX: f32 = 12.0;
/// Font size of the heading, in pixels.
pub const HEADING_FONT_PX: f32 = 11.0;
/// Font size of the mining note, in pixels.
pub const NOTE_FONT_PX: f32 = 11.0;

/// Upper bound on one character's advance width, as a fraction of font size.
///
/// Bevy's default face is FiraMono, which is monospaced at 0.6 em. The margin
/// over that is deliberate: `legend_fits_the_default_720_line_window` is a
/// *fit* proof, so it has to over-estimate the text rather than under-estimate
/// it, and a slightly wider panel costs nothing while a clipped row costs the
/// whole feature.
pub const CHAR_ADVANCE_RATIO: f32 = 0.62;

/// Width of the left-hand column, which holds the `keys` text.
pub const KEYS_COLUMN_PX: f32 = 96.0;
/// Gap between the two columns.
pub const COLUMN_GAP_PX: f32 = 10.0;
/// Padding inside the panel, on every side.
pub const PADDING_PX: f32 = 10.0;
/// Overall panel width.
///
/// Authored, not derived. A width computed from `ROWS` would make the fit test
/// a restatement of the layout code; this way the two are separate sources and
/// the test can fail.
pub const WIDTH_PX: f32 = 322.0;
/// How far the panel sits from the bottom and right edges.
pub const MARGIN_PX: f32 = 22.0;
/// Vertical gap between rows.
pub const ROW_GAP_PX: f32 = 3.0;
/// Height one text line occupies, including leading.
pub const LINE_HEIGHT_RATIO: f32 = 1.25;

/// How long the legend stays up when the player never touches an input.
pub const AUTOHIDE_SECS: f32 = 90.0;

/// Rough rendered width of `text` at `font_px`, over-estimated on purpose.
///
/// See [`CHAR_ADVANCE_RATIO`]. Bevy knows the true width only after layout has
/// run, which is a frame too late to place anything from.
#[must_use]
pub fn text_width_px(text: &str, font_px: f32) -> f32 {
    text.chars().count() as f32 * font_px * CHAR_ADVANCE_RATIO
}

/// Panel width minus its padding: what the two columns have to live inside.
#[must_use]
pub fn content_width_px() -> f32 {
    WIDTH_PX - 2.0 * PADDING_PX
}

/// Width available to a row's `action` text.
#[must_use]
pub fn action_column_px() -> f32 {
    content_width_px() - KEYS_COLUMN_PX - COLUMN_GAP_PX
}

/// Panel height, from the lines it draws.
///
/// Heading, every row, then the mining note, with `ROW_GAP_PX` between each.
#[must_use]
pub fn height_px() -> f32 {
    let lines = 3.0 + ROWS.len() as f32;
    let text = (HEADING_FONT_PX + 2.0 * NOTE_FONT_PX + ROWS.len() as f32 * ROW_FONT_PX)
        * LINE_HEIGHT_RATIO;
    text + (lines - 1.0) * ROW_GAP_PX + 2.0 * PADDING_PX
}

/// Which inputs this player has used, and whether the panel is up.
///
/// Nothing here is simulation state: it is a record of what reached the
/// keyboard and the mouse in this process.
#[derive(Debug, Resource)]
pub struct LegendState {
    used: [bool; BOUND.len()],
    elapsed_secs: f32,
    /// `Some` once the player has pressed F1: their choice outranks the
    /// automatic retirement, in both directions.
    manual: Option<bool>,
}

impl Default for LegendState {
    fn default() -> Self {
        Self {
            used: [false; BOUND.len()],
            elapsed_secs: 0.0,
            manual: None,
        }
    }
}

impl LegendState {
    /// Record that `bound` was used this frame.
    pub fn mark(&mut self, bound: Bound) {
        if let Some(index) = BOUND.iter().position(|candidate| *candidate == bound) {
            self.used[index] = true;
        }
    }

    /// Whether `bound` has been used at least once.
    #[must_use]
    pub fn used(&self, bound: Bound) -> bool {
        BOUND
            .iter()
            .position(|candidate| *candidate == bound)
            .is_some_and(|index| self.used[index])
    }

    /// Advance the retirement clock.
    pub fn tick(&mut self, delta_secs: f32) {
        self.elapsed_secs += delta_secs.max(0.0);
    }

    /// Flip the panel, and hold that choice from now on.
    pub fn toggle(&mut self) {
        self.manual = Some(!self.visible());
    }

    /// Whether the panel should be drawn this frame.
    #[must_use]
    pub fn visible(&self) -> bool {
        if let Some(manual) = self.manual {
            return manual;
        }
        !self.retired()
    }

    /// Whether the panel has retired itself: every flight input demonstrated,
    /// or the player has been in the seat long enough to have found them.
    #[must_use]
    pub fn retired(&self) -> bool {
        self.elapsed_secs >= AUTOHIDE_SECS || FLIGHT.iter().all(|bound| self.used(*bound))
    }
}

/// The legend panel's root node.
#[derive(Component)]
pub struct LegendPanel;

/// One legend row, tagged with the binding it describes so it can dim once
/// that binding has been used.
#[derive(Component)]
pub struct LegendRow(
    /// The binding this row names.
    pub Bound,
);

fn row_bundle(row: Row) -> impl Bundle {
    (
        Node {
            flex_direction: FlexDirection::Row,
            column_gap: Val::Px(COLUMN_GAP_PX),
            ..Default::default()
        },
        LegendRow(row.bound),
        children![
            (
                Node {
                    width: Val::Px(KEYS_COLUMN_PX),
                    ..Default::default()
                },
                children![(
                    Text::new(row.keys),
                    TextFont::from_font_size(ROW_FONT_PX),
                    TextColor(MUTED),
                )]
            ),
            (
                Text::new(row.action),
                TextFont::from_font_size(ROW_FONT_PX),
                TextColor(DIM),
            ),
        ],
    )
}

/// Spawns the legend in the bottom-right corner.
///
/// That corner is the one the HUD leaves empty: the weapon and craft panels
/// are bottom-left, the lock panel is top-right, the session banner is
/// top-centre and the always-on strip is top-left.
pub fn spawn_legend(commands: &mut Commands) {
    let mut root = commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            right: Val::Px(MARGIN_PX),
            bottom: Val::Px(MARGIN_PX),
            width: Val::Px(WIDTH_PX),
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(ROW_GAP_PX),
            padding: UiRect::all(Val::Px(PADDING_PX)),
            border_radius: BorderRadius::all(Val::Px(8.0)),
            ..Default::default()
        },
        BackgroundColor(PANEL),
        GlobalZIndex(90),
        LegendPanel,
    ));
    root.with_children(|panel| {
        panel.spawn((
            Text::new(HEADING),
            TextFont::from_font_size(HEADING_FONT_PX),
            TextColor(ACCENT_PALE),
        ));
        for row in ROWS {
            panel.spawn(row_bundle(row));
        }
        panel.spawn((
            Text::new(NOTE),
            TextFont::from_font_size(NOTE_FONT_PX),
            TextColor(DIM),
        ));
        panel.spawn((
            Text::new(PICKUP_NOTE),
            TextFont::from_font_size(NOTE_FONT_PX),
            TextColor(DIM),
        ));
    });
}

/// Records which inputs the player has demonstrated, and ages the panel out.
///
/// Reads the same `ButtonInput` state the intent path reads and writes nothing
/// back to it, so noticing an input can never consume one.
pub fn note_used_inputs(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut wheel: MessageReader<bevy::input::mouse::MouseWheel>,
    mut state: ResMut<LegendState>,
) {
    state.tick(time.delta_secs());
    if keys.pressed(KeyCode::ArrowLeft) || keys.pressed(KeyCode::ArrowRight) {
        state.mark(Bound::Yaw);
    }
    if keys.pressed(KeyCode::ArrowUp) {
        state.mark(Bound::Thrust);
    }
    if keys.pressed(KeyCode::Space) {
        state.mark(Bound::Fire);
    }
    if buttons.just_pressed(MouseButton::Left) {
        state.mark(Bound::Select);
    }
    if keys.just_pressed(KeyCode::F3) {
        state.mark(Bound::Overlay);
    }
    if wheel.read().next().is_some() {
        state.mark(Bound::Zoom);
    }
}

/// `F1` shows or hides the legend.
pub fn toggle_legend(keys: Res<ButtonInput<KeyCode>>, mut state: ResMut<LegendState>) {
    if keys.just_pressed(KeyCode::F1) {
        state.toggle();
    }
}

/// Draws the panel's current state: visible or not, and which rows are spent.
///
/// A row the player has used goes faint rather than vanishing, so the panel
/// keeps its shape while it drains — a list that reflows under the reader is
/// harder to use than one that fades.
pub fn sync_legend(
    state: Res<LegendState>,
    mut panel: Query<&mut Visibility, With<LegendPanel>>,
    rows: Query<(&LegendRow, &Children)>,
    mut colours: Query<&mut TextColor>,
) {
    if !state.is_changed() {
        return;
    }
    let wanted = if state.visible() {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    for mut visibility in &mut panel {
        if *visibility != wanted {
            *visibility = wanted;
        }
    }
    for (row, children) in &rows {
        let spent = state.used(row.0);
        for child in children {
            if let Ok(mut colour) = colours.get_mut(*child) {
                let wanted = if spent { FAINT } else { DIM };
                if colour.0 != wanted {
                    *colour = TextColor(wanted);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::Controls;
    use orrery_protocol::PersistId;

    /// Every `KeyCode` a sweep test presses, one at a time.
    ///
    /// Not exhaustive over `KeyCode` — it has no iterator — but it covers
    /// every key a person would plausibly reach for on a keyboard whose game
    /// gave them no legend, which is exactly the population #564 is about.
    fn sweep() -> Vec<(&'static str, KeyCode)> {
        vec![
            ("KeyW", KeyCode::KeyW),
            ("KeyA", KeyCode::KeyA),
            ("KeyS", KeyCode::KeyS),
            ("KeyD", KeyCode::KeyD),
            ("KeyE", KeyCode::KeyE),
            ("KeyQ", KeyCode::KeyQ),
            ("KeyF", KeyCode::KeyF),
            ("KeyR", KeyCode::KeyR),
            ("KeyG", KeyCode::KeyG),
            ("KeyM", KeyCode::KeyM),
            ("KeyX", KeyCode::KeyX),
            ("KeyZ", KeyCode::KeyZ),
            ("KeyC", KeyCode::KeyC),
            ("KeyV", KeyCode::KeyV),
            ("KeyB", KeyCode::KeyB),
            ("Digit1", KeyCode::Digit1),
            ("Digit2", KeyCode::Digit2),
            ("Digit3", KeyCode::Digit3),
            ("Digit0", KeyCode::Digit0),
            ("ArrowLeft", KeyCode::ArrowLeft),
            ("ArrowRight", KeyCode::ArrowRight),
            ("ArrowUp", KeyCode::ArrowUp),
            ("ArrowDown", KeyCode::ArrowDown),
            ("Space", KeyCode::Space),
            ("Enter", KeyCode::Enter),
            ("Tab", KeyCode::Tab),
            ("Escape", KeyCode::Escape),
            ("Backspace", KeyCode::Backspace),
            ("ShiftLeft", KeyCode::ShiftLeft),
            ("ControlLeft", KeyCode::ControlLeft),
            ("AltLeft", KeyCode::AltLeft),
            ("F1", KeyCode::F1),
            ("F2", KeyCode::F2),
            ("F3", KeyCode::F3),
            ("F4", KeyCode::F4),
            ("F5", KeyCode::F5),
            ("F6", KeyCode::F6),
            ("F7", KeyCode::F7),
            ("F8", KeyCode::F8),
            ("F9", KeyCode::F9),
            ("F10", KeyCode::F10),
            ("F11", KeyCode::F11),
            ("F12", KeyCode::F12),
        ]
    }

    /// The keys one legend row names, read back out of the row's own text.
    ///
    /// This is the join that makes the binding tests two-source: the row is
    /// prose written for a player, and the test recovers `KeyCode`s from that
    /// prose and presses them. Rewrite a row to say `W` and it stops naming a
    /// key the client reads, which is a failure, not a silent pass.
    fn keys_named_by(row: Row) -> Vec<KeyCode> {
        row.keys
            .split('/')
            .filter_map(|word| {
                let word = word.trim();
                sweep()
                    .into_iter()
                    .find(|(name, _)| {
                        name.eq_ignore_ascii_case(word)
                            || name.eq_ignore_ascii_case(&format!("Arrow{word}"))
                    })
                    .map(|(_, code)| code)
            })
            .collect()
    }

    fn pressed(codes: &[KeyCode]) -> ButtonInput<KeyCode> {
        let mut input = ButtonInput::default();
        for code in codes {
            input.press(*code);
        }
        input
    }

    #[test]
    fn legend_text_is_ascii_only() {
        let mut strings = vec![HEADING, NOTE, PICKUP_NOTE];
        for row in ROWS {
            strings.push(row.keys);
            strings.push(row.action);
        }
        for text in strings {
            assert!(
                text.chars().all(|c| c.is_ascii() && !c.is_ascii_control()),
                "legend text {text:?} leaves ASCII; Bevy draws that as empty boxes"
            );
        }
    }

    #[test]
    fn every_binding_has_exactly_one_row() {
        for bound in BOUND {
            let rows = ROWS.iter().filter(|row| row.bound == bound).count();
            assert_eq!(rows, 1, "{bound:?} is named by {rows} legend rows, want 1");
        }
        assert_eq!(ROWS.len(), BOUND.len());
    }

    /// The fit proof, at the default window Bevy opens: 1280x720.
    ///
    /// #552 records that 720 lines is already tight, so this is numeric rather
    /// than a look: every string is measured at its own font size against the
    /// box it is drawn in, and the panel is measured against the two HUD
    /// clusters it must not reach.
    #[test]
    fn legend_fits_the_default_720_line_window() {
        const WINDOW_W: f32 = 1280.0;
        const WINDOW_H: f32 = 720.0;
        // `hud::spawn_hud`: 22 px in, then 340 + 11 + 268 of panel.
        const HUD_BOTTOM_LEFT_RIGHT_EDGE_PX: f32 = 22.0 + 340.0 + 11.0 + 268.0;

        for row in ROWS {
            let keys = text_width_px(row.keys, ROW_FONT_PX);
            assert!(
                keys <= KEYS_COLUMN_PX,
                "row {:?} keys {:?} measure {keys:.1} px, column is {KEYS_COLUMN_PX:.1} px",
                row.bound,
                row.keys
            );
            let action = text_width_px(row.action, ROW_FONT_PX);
            assert!(
                action <= action_column_px(),
                "row {:?} action {:?} measures {action:.1} px, column is {:.1} px",
                row.bound,
                row.action,
                action_column_px()
            );
        }

        let heading = text_width_px(HEADING, HEADING_FONT_PX);
        assert!(
            heading <= content_width_px(),
            "heading measures {heading:.1} px inside {:.1} px",
            content_width_px()
        );
        for (what, note) in [("mining", NOTE), ("pickup", PICKUP_NOTE)] {
            let width = text_width_px(note, NOTE_FONT_PX);
            assert!(
                width <= content_width_px(),
                "the {what} note measures {width:.1} px inside {:.1} px",
                content_width_px()
            );
        }

        let left_edge = WINDOW_W - MARGIN_PX - WIDTH_PX;
        assert!(
            left_edge > HUD_BOTTOM_LEFT_RIGHT_EDGE_PX,
            "the legend starts at {left_edge:.1} px and the bottom-left HUD ends at \
             {HUD_BOTTOM_LEFT_RIGHT_EDGE_PX:.1} px: they would overlap"
        );

        let top_edge = WINDOW_H - MARGIN_PX - height_px();
        assert!(
            top_edge > 0.0,
            "the legend is {:.1} px tall and would run off a {WINDOW_H:.0}-line window",
            height_px()
        );
        // `hud::spawn_hud`'s lock panel is top-right, 22 px down, and is nine
        // text rows and two gauges deep. 320 px is a generous bound on it.
        assert!(
            top_edge > 22.0 + 320.0,
            "the legend's top edge at {top_edge:.1} px reaches the top-right lock panel"
        );
    }

    /// Every key the legend names really drives the binding it claims.
    ///
    /// The mutation this is written to catch is a binding moving in
    /// `crate::controls` while the legend keeps saying the old thing. Pressing
    /// the key the row names and asserting the effect is the only way that
    /// fails; comparing the legend against a table it was built from is not.
    #[test]
    fn every_legend_row_drives_the_binding_it_names() {
        for row in ROWS {
            let codes = keys_named_by(row);
            match row.bound {
                Bound::Select | Bound::Zoom => {
                    assert!(
                        codes.is_empty(),
                        "row {:?} is a mouse binding but names key codes {codes:?}",
                        row.bound
                    );
                    let lowered = row.keys.to_ascii_lowercase();
                    let expected = if row.bound == Bound::Select {
                        "click"
                    } else {
                        "wheel"
                    };
                    assert!(
                        lowered.contains(expected),
                        "row {:?} should tell the player about the {expected}, says {:?}",
                        row.bound,
                        row.keys
                    );
                }
                _ => assert!(
                    !codes.is_empty(),
                    "row {:?} names {:?}, which is not a key this client reads",
                    row.bound,
                    row.keys
                ),
            }

            for code in codes {
                let controls = crate::controls(&pressed(&[code]), None);
                match row.bound {
                    Bound::Yaw => assert!(
                        controls.left ^ controls.right,
                        "{code:?} is named as a turn key and turns nothing"
                    ),
                    Bound::Thrust => assert!(
                        controls.thrust,
                        "{code:?} is named as thrust and does not thrust"
                    ),
                    Bound::Fire => {
                        assert!(controls.fire, "{code:?} is named as fire and does not fire");
                    }
                    Bound::Overlay => {
                        let mut app = App::new();
                        app.init_resource::<crate::OverlayState>()
                            .insert_resource(pressed(&[code]))
                            .add_systems(Update, crate::toggle_overlay);
                        app.update();
                        assert!(
                            app.world().resource::<crate::OverlayState>().expanded,
                            "{code:?} is named as the overlay key and toggles nothing"
                        );
                    }
                    Bound::Legend => {
                        let before = LegendState::default().visible();
                        let mut app = App::new();
                        app.insert_resource(LegendState::default())
                            .insert_resource(pressed(&[code]))
                            .add_systems(Update, toggle_legend);
                        app.update();
                        assert_ne!(
                            app.world().resource::<LegendState>().visible(),
                            before,
                            "{code:?} is named as the legend key and hides nothing"
                        );
                    }
                    Bound::Select | Bound::Zoom => unreachable!("mouse rows name no key"),
                }
            }
        }
    }

    /// Nothing is bound that the legend does not name.
    ///
    /// The other direction of the same obligation: a key added to
    /// `crate::controls` or to a toggle without a legend row leaves a player
    /// with an input they cannot discover, which is the bug #564 is about.
    #[test]
    fn no_keyboard_binding_is_missing_from_the_legend() {
        let named: Vec<KeyCode> = ROWS.into_iter().flat_map(keys_named_by).collect();
        for (name, code) in sweep() {
            if named.contains(&code) {
                continue;
            }
            assert_eq!(
                crate::controls(&pressed(&[code]), None),
                Controls::default(),
                "{name} changes the craft's controls and no legend row names it"
            );

            let mut app = App::new();
            app.init_resource::<crate::OverlayState>()
                .init_resource::<LegendState>()
                .insert_resource(pressed(&[code]))
                .add_systems(Update, (crate::toggle_overlay, toggle_legend));
            app.update();
            assert!(
                !app.world().resource::<crate::OverlayState>().expanded,
                "{name} opens the correctness overlay and no legend row names it"
            );
            assert!(
                app.world().resource::<LegendState>().visible(),
                "{name} hides the legend and no legend row names it"
            );
        }
    }

    /// The click row is about a real selection, not decoration.
    #[test]
    fn the_click_row_describes_what_clicking_does() {
        let cursor = Vec2::new(100.0, 100.0);
        let near = PersistId::new(7);
        let far = PersistId::new(9);
        let picked = crate::nearest_clicked(
            cursor,
            [
                (far, Vec2::new(120.0, 120.0)),
                (near, Vec2::new(104.0, 100.0)),
            ]
            .into_iter(),
        );
        assert_eq!(picked, Some(near), "clicking picks the nearest body");
    }

    /// The wheel row is about a real zoom.
    #[test]
    fn the_wheel_row_describes_what_the_wheel_does() {
        let start = crate::CameraZoom::default();
        assert!(
            start.zoomed(1.0).height_m() < start.height_m(),
            "a wheel notch away from the player zooms in"
        );
        assert!(
            start.zoomed(-1.0).height_m() > start.height_m(),
            "a wheel notch toward the player zooms out"
        );
    }

    #[test]
    fn the_legend_retires_once_every_flight_input_has_been_used() {
        let mut state = LegendState::default();
        assert!(state.visible(), "a fresh session shows the legend");
        for bound in FLIGHT {
            assert!(state.visible(), "{bound:?} is still undemonstrated");
            state.mark(bound);
        }
        assert!(
            !state.visible(),
            "the legend should retire once the player has flown, fired and looked around"
        );
    }

    #[test]
    fn the_legend_retires_on_its_own_clock_too() {
        let mut state = LegendState::default();
        state.tick(AUTOHIDE_SECS - 1.0);
        assert!(state.visible());
        state.tick(1.0);
        assert!(!state.visible(), "the legend ages out of an idle session");
    }

    #[test]
    fn f1_outranks_the_automatic_retirement_in_both_directions() {
        let mut state = LegendState::default();
        state.tick(AUTOHIDE_SECS * 2.0);
        assert!(!state.visible());
        state.toggle();
        assert!(state.visible(), "F1 brings a retired legend back");
        state.toggle();
        assert!(!state.visible(), "F1 puts it away again");

        let mut fresh = LegendState::default();
        fresh.toggle();
        assert!(!fresh.visible(), "F1 dismisses a legend that is still new");
        for bound in FLIGHT {
            fresh.mark(bound);
        }
        assert!(!fresh.visible(), "and it stays dismissed");
    }
}
