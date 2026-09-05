//! The engine side of the crossing: a link that reads `SidecarToEngine`
//! frames off the byte stream and keeps the presented set an engine renders
//! (#898 step 3).
//!
//! # Why the observer's state lives here and not in the engine
//!
//! A9 §2.4 gives the extraction copy-out, overwrite semantics: the consumer
//! applies the newest batch and holds no history to unwind. That rule is
//! cheap to state and easy to get wrong once per engine — a C++ renderer that
//! accumulates, or that keys presentation on its own actor handles rather
//! than on [`PersistId`], reintroduces exactly the coupling the schema exists
//! to prevent. So the rule is implemented once, in Rust, beside the framing
//! that delivers the bytes, and every engine binding is a projection of
//! [`ObserverView`] rather than a second implementation of it.
//!
//! This module stays Bevy-free with the rest of the crate: it names the
//! codec, the framing and `std::net`, and nothing else. The sidecar is the
//! only side with an ECS.
//!
//! # Membership comes from the frames batch, not from spawn/despawn
//!
//! `orrery::ipc::export_ipc_frames` iterates its whole predicted and
//! interpolated queries on every run, so a [`FrameBatch`] is a *complete*
//! extraction of the presentation set, not a delta. Membership is therefore
//! rebuilt from it, and the [`SpawnBatch`]/[`DespawnBatch`] messages are
//! counted as the announced diff rather than applied — applying them could
//! only ever agree with the extraction that produced them, and where a
//! dropped or reordered message made them disagree, the batch that carries
//! the actual transforms is the one to believe. An observer that connects
//! mid-run is correct from its first frames batch for the same reason, with
//! no join protocol.
//!
//! Corrections are notices, not state: [`SidecarToEngine::Corrections`] tells
//! the renderer that a predicted entity's timeline was replayed, and the
//! frames batch beside it already carries the regenerated transform. The view
//! records the tick on the entity so a renderer can suppress smoothing for a
//! frame; it never rewinds anything.

extern crate alloc;

use alloc::collections::BTreeMap;
use std::io::{self, Read};
use std::net::{TcpStream, ToSocketAddrs};

use orrery_ipc::{DecodeError, EntityFrame, QuantizedTransform, SidecarToEngine};
use orrery_protocol::{InterpBasis, PersistId, Tick};

use crate::{set_nodelay, FrameReader};

/// Which of the two timelines presented an entity.
///
/// The distinction is the renderer's whole reason to care: a predicted entity
/// is this peer's own optimistic future and carries an exact basis, while an
/// interpolated one is a remote peer's confirmed past rendered between two
/// snapshots. #898 step 3 asks for one capsule of each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Timeline {
    /// Locally predicted: this peer's own speculative timeline.
    Predicted,
    /// Remotely interpolated: a confirmed timeline rendered between snapshots.
    Interpolated,
}

/// One entity as the observer last saw it presented.
///
/// Every field is a copy-out of the batch that carried it. There is no
/// engine-native handle here and no history: a renderer that needs an actor
/// handle keeps its own map keyed on [`PersistId`], which is the only
/// identity the schema admits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PresentedEntity {
    /// Which timeline presented it in the most recent batch.
    pub timeline: Timeline,
    /// The transform the sidecar actually presented.
    pub transform: QuantizedTransform,
    /// The basis that transform was produced on.
    pub basis: InterpBasis,
    /// The extraction tick of the batch that carried it.
    pub presented_at: Tick,
    /// The tick of the most recent correction notice for this entity, if the
    /// link has seen one. A renderer may use it to skip a frame of smoothing.
    pub corrected_at: Option<Tick>,
}

/// What one link has been told about the sidecar's presentation set.
///
/// Overwrite semantics throughout: [`apply`](Self::apply) replaces, never
/// accumulates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ObserverView {
    entities: BTreeMap<PersistId, PresentedEntity>,
    last_frame_tick: Option<Tick>,
    frames_applied: u64,
    spawns_announced: u64,
    despawns_announced: u64,
    corrections_announced: u64,
}

impl ObserverView {
    /// An empty view, before the first batch arrives.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one message, replacing whatever it supersedes.
    pub fn apply(&mut self, message: &SidecarToEngine) {
        match message {
            SidecarToEngine::Frames(batch) => {
                self.entities = batch
                    .predicted
                    .iter()
                    .map(|frame| self.presented(frame, Timeline::Predicted, batch.extracted_at))
                    .chain(batch.interpolated.iter().map(|frame| {
                        self.presented(frame, Timeline::Interpolated, batch.extracted_at)
                    }))
                    .collect();
                self.last_frame_tick = Some(batch.extracted_at);
                self.frames_applied += 1;
            }
            SidecarToEngine::Spawns(batch) => {
                self.spawns_announced += batch.entities.len() as u64;
            }
            SidecarToEngine::Despawns(batch) => {
                self.despawns_announced += batch.entities.len() as u64;
            }
            SidecarToEngine::Corrections(batch) => {
                self.corrections_announced += batch.corrections.len() as u64;
                for notice in &batch.corrections {
                    if let Some(entity) = self.entities.get_mut(&notice.persist_id) {
                        entity.corrected_at = Some(notice.observed_at);
                    }
                }
            }
        }
    }

    /// Carry a correction stamp across a rebuild of the presentation set: the
    /// notice is about the entity, not about the batch it arrived beside.
    fn presented(
        &self,
        frame: &EntityFrame,
        timeline: Timeline,
        extracted_at: Tick,
    ) -> (PersistId, PresentedEntity) {
        (
            frame.persist_id,
            PresentedEntity {
                timeline,
                transform: frame.transform,
                basis: frame.basis,
                presented_at: extracted_at,
                corrected_at: self
                    .entities
                    .get(&frame.persist_id)
                    .and_then(|previous| previous.corrected_at),
            },
        )
    }

    /// The presentation set, in stable-id order.
    #[must_use]
    pub fn entities(&self) -> impl ExactSizeIterator<Item = (&PersistId, &PresentedEntity)> {
        self.entities.iter()
    }

    /// How many entities are presented.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Whether nothing is presented.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// One entity by stable id.
    #[must_use]
    pub fn get(&self, id: PersistId) -> Option<&PresentedEntity> {
        self.entities.get(&id)
    }

    /// The extraction tick of the most recent frames batch.
    #[must_use]
    pub const fn last_frame_tick(&self) -> Option<Tick> {
        self.last_frame_tick
    }

    /// How many frames batches have been applied.
    #[must_use]
    pub const fn frames_applied(&self) -> u64 {
        self.frames_applied
    }

    /// How many spawns the sidecar announced.
    #[must_use]
    pub const fn spawns_announced(&self) -> u64 {
        self.spawns_announced
    }

    /// How many despawns the sidecar announced.
    #[must_use]
    pub const fn despawns_announced(&self) -> u64 {
        self.despawns_announced
    }

    /// How many correction notices the sidecar announced.
    #[must_use]
    pub const fn corrections_announced(&self) -> u64 {
        self.corrections_announced
    }
}

/// Why a link stopped carrying frames.
#[derive(Debug)]
pub enum LinkError {
    /// The byte stream failed, or ended inside a frame.
    Io(io::Error),
    /// A complete frame arrived that is not a valid sidecar-to-engine message.
    Decode(DecodeError),
}

impl core::fmt::Display for LinkError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "observer link I/O failed: {error}"),
            Self::Decode(error) => write!(formatter, "observer link decode failed: {error}"),
        }
    }
}

impl core::error::Error for LinkError {}

impl From<io::Error> for LinkError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<DecodeError> for LinkError {
    fn from(error: DecodeError) -> Self {
        Self::Decode(error)
    }
}

/// What one [`ObserverLink::poll`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polled {
    /// A message arrived and was applied to the view.
    Applied,
    /// The sidecar closed the link cleanly at a frame boundary.
    Closed,
}

/// One link to one sidecar: framed reads, decoded, applied to a view.
///
/// A link is deliberately single-sidecar. #898 step 3 renders *two* sidecars,
/// and the observer holds two of these rather than the library multiplexing
/// them: an engine that wants a third adds a third link, and nothing in the
/// schema, the framing or the view has to learn what a peer set is.
pub struct ObserverLink<R: Read> {
    reader: FrameReader<R>,
    view: ObserverView,
}

impl ObserverLink<TcpStream> {
    /// Dial a serving sidecar.
    ///
    /// `TCP_NODELAY` is set before a byte moves, for the reason #920 lie 2
    /// gives: Nagle would batch small frames and put ~40 ms of the operating
    /// system's opinion into a 60 Hz presentation stream.
    ///
    /// # Errors
    ///
    /// Returns the connect error, or the error from setting `TCP_NODELAY`.
    pub fn connect(addr: impl ToSocketAddrs) -> io::Result<Self> {
        let stream = TcpStream::connect(addr)?;
        set_nodelay(&stream)?;
        Ok(Self::new(stream))
    }
}

impl<R: Read> ObserverLink<R> {
    /// Wrap any framed byte source.
    #[must_use]
    pub fn new(inner: R) -> Self {
        Self {
            reader: FrameReader::new(inner),
            view: ObserverView::new(),
        }
    }

    /// Read and apply exactly one message, blocking until one is complete.
    ///
    /// # Errors
    ///
    /// Returns [`LinkError::Io`] when the stream fails or ends mid-frame, and
    /// [`LinkError::Decode`] when a complete frame is not a valid message.
    /// Both are fatal for the link: the stream has no resync point, so the
    /// caller drops it and reconnects.
    pub fn poll(&mut self) -> Result<Polled, LinkError> {
        let Some(body) = self.reader.read_frame()? else {
            return Ok(Polled::Closed);
        };
        let message = SidecarToEngine::decode(&body)?;
        self.view.apply(&message);
        Ok(Polled::Applied)
    }

    /// The presentation set this link has been told about.
    #[must_use]
    pub const fn view(&self) -> &ObserverView {
        &self.view
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orrery_ipc::{CorrectionBatch, CorrectionNotice, DespawnBatch, FrameBatch, SpawnBatch};
    use orrery_protocol::{LatticePoint, QuantizedDir, UNorm16};

    use crate::FrameWriter;

    fn transform(x: i64) -> QuantizedTransform {
        QuantizedTransform {
            translation: LatticePoint::new(x, 0, 0),
            forward: QuantizedDir::new(1, 0, 0),
            up: QuantizedDir::new(0, 1, 0),
        }
    }

    fn frame(id: u64, x: i64, basis: InterpBasis) -> EntityFrame {
        EntityFrame {
            persist_id: PersistId::new(id),
            transform: transform(x),
            basis,
        }
    }

    fn frames(
        tick: u64,
        predicted: Vec<EntityFrame>,
        interpolated: Vec<EntityFrame>,
    ) -> SidecarToEngine {
        SidecarToEngine::Frames(FrameBatch {
            extracted_at: Tick::new(tick),
            predicted,
            interpolated,
        })
    }

    const LOCAL: PersistId = PersistId::new(1);
    const REMOTE: PersistId = PersistId::new(2);

    /// The two capsules #898 step 3 renders, told apart by the class that
    /// presented them, and the interpolated one keeping the real bracket.
    #[test]
    fn the_view_separates_the_predicted_capsule_from_the_interpolated_one() {
        let bracket = InterpBasis {
            from: Tick::new(90),
            to: Tick::new(96),
            alpha: UNorm16(16_384),
        };
        let mut view = ObserverView::new();
        view.apply(&frames(
            100,
            vec![frame(1, 10, InterpBasis::exact(Tick::new(100)))],
            vec![frame(2, 20, bracket)],
        ));

        assert_eq!(view.len(), 2);
        let local = view.get(LOCAL).expect("the predicted capsule");
        assert_eq!(local.timeline, Timeline::Predicted);
        assert_eq!(local.transform.translation.x, 10);
        assert_eq!(local.basis, InterpBasis::exact(Tick::new(100)));

        let remote = view.get(REMOTE).expect("the interpolated capsule");
        assert_eq!(remote.timeline, Timeline::Interpolated);
        assert_eq!(remote.transform.translation.x, 20);
        assert_eq!(
            remote.basis, bracket,
            "the renderer is handed the bracket the sidecar presented on"
        );
        assert_eq!(view.last_frame_tick(), Some(Tick::new(100)));
    }

    /// Overwrite semantics, A9 §2.4(3): the newest batch replaces the set. An
    /// entity absent from a complete extraction is gone from presentation
    /// even if no despawn batch said so.
    #[test]
    fn a_later_batch_replaces_the_set_rather_than_accumulating() {
        let mut view = ObserverView::new();
        view.apply(&frames(
            100,
            vec![frame(1, 10, InterpBasis::exact(Tick::new(100)))],
            vec![frame(2, 20, InterpBasis::exact(Tick::new(100)))],
        ));
        view.apply(&frames(
            101,
            vec![frame(1, 11, InterpBasis::exact(Tick::new(101)))],
            Vec::new(),
        ));

        assert_eq!(
            view.len(),
            1,
            "the set is the newest extraction, not a union"
        );
        assert_eq!(
            view.get(LOCAL)
                .expect("still presented")
                .transform
                .translation
                .x,
            11
        );
        assert_eq!(view.get(REMOTE), None);
        assert_eq!(view.frames_applied(), 2);
    }

    /// A spawn/despawn batch is the announced diff, counted but never applied:
    /// the frames batch beside it is the complete extraction and decides
    /// membership. Here the diff is deliberately made to disagree, and the
    /// extraction wins.
    #[test]
    fn membership_follows_the_extraction_and_not_the_announced_diff() {
        let mut view = ObserverView::new();
        view.apply(&frames(
            100,
            vec![frame(1, 10, InterpBasis::exact(Tick::new(100)))],
            Vec::new(),
        ));
        view.apply(&SidecarToEngine::Despawns(DespawnBatch {
            entities: vec![LOCAL],
        }));
        view.apply(&SidecarToEngine::Spawns(SpawnBatch {
            entities: vec![REMOTE],
        }));

        assert!(
            view.get(LOCAL).is_some(),
            "a despawn notice does not remove what the extraction still presents"
        );
        assert!(
            view.get(REMOTE).is_none(),
            "a spawn notice does not present what the extraction did not carry"
        );
        assert_eq!(view.despawns_announced(), 1);
        assert_eq!(view.spawns_announced(), 1);
    }

    /// A correction is a notice about an entity, not about the batch it came
    /// beside: it survives the next extraction's rebuild of the set.
    #[test]
    fn a_correction_notice_stamps_the_entity_and_survives_the_next_batch() {
        let mut view = ObserverView::new();
        view.apply(&frames(
            100,
            vec![frame(1, 10, InterpBasis::exact(Tick::new(100)))],
            Vec::new(),
        ));
        view.apply(&SidecarToEngine::Corrections(CorrectionBatch {
            corrections: vec![CorrectionNotice {
                persist_id: LOCAL,
                observed_at: Tick::new(97),
            }],
        }));
        assert_eq!(
            view.get(LOCAL).expect("presented").corrected_at,
            Some(Tick::new(97))
        );

        view.apply(&frames(
            101,
            vec![frame(1, 11, InterpBasis::exact(Tick::new(101)))],
            Vec::new(),
        ));
        let local = view.get(LOCAL).expect("presented");
        assert_eq!(
            local.corrected_at,
            Some(Tick::new(97)),
            "the stamp belongs to the entity, and the regenerated frame is applied by overwrite"
        );
        assert_eq!(local.transform.translation.x, 11);
        assert_eq!(view.corrections_announced(), 1);
    }

    /// The link reads real framed bytes off a stream, and reports a clean end
    /// of stream as `Closed` rather than as an error.
    #[test]
    fn the_link_decodes_framed_bytes_and_reports_a_clean_close() {
        let mut bytes = Vec::new();
        {
            let mut writer = FrameWriter::new(&mut bytes);
            for tick in 100..103_u64 {
                let body = frames(
                    tick,
                    vec![frame(
                        1,
                        i64::try_from(tick).expect("tick fits"),
                        InterpBasis::exact(Tick::new(tick)),
                    )],
                    Vec::new(),
                )
                .encode()
                .expect("batch encodes");
                writer.write_frame(&body).expect("frame writes");
            }
        }

        let mut link = ObserverLink::new(bytes.as_slice());
        for _ in 0..3 {
            assert_eq!(link.poll().expect("a frame arrives"), Polled::Applied);
        }
        assert_eq!(link.view().frames_applied(), 3);
        assert_eq!(
            link.view()
                .get(LOCAL)
                .expect("presented")
                .transform
                .translation
                .x,
            102
        );
        assert_eq!(link.poll().expect("clean end of stream"), Polled::Closed);
    }

    /// A complete frame that is not a valid message is fatal for the link:
    /// the stream has no resync, so the caller must drop it.
    #[test]
    fn a_bad_frame_is_a_fatal_link_error() {
        let mut bytes = Vec::new();
        {
            let mut writer = FrameWriter::new(&mut bytes);
            writer.write_frame(b"not a message").expect("frame writes");
        }
        let mut link = ObserverLink::new(bytes.as_slice());
        assert!(matches!(link.poll(), Err(LinkError::Decode(_))));
    }
}
