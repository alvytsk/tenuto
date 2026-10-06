//! Opening the profile's `state.json` into a session and its writer: the
//! one path both front ends take (M9.2). The caller has already taken the
//! profile lock and called `StateStore::load` — the terminal player loads
//! before it redirects stderr — so loading and opening are separate steps.

use std::sync::Arc;

use crate::clock::Clock;
use crate::persistence::store::{LoadOutcome, LoadReason, QueueBackup, QueueRepair, StateStore};
use crate::persistence::writer::{DisabledSink, StateSink, WriterHandle};
use crate::session::Session;

pub struct OpenedState {
    pub session: Session,
    pub writer: WriterHandle,
    /// Whether anything `writer` accepts can reach the disk. A disabled sink
    /// reports every write as a success, deliberately (D11), so this is what
    /// keeps a shutdown from claiming a write that never happened.
    pub persisting: bool,
    /// A queue repair the load performed, for a front end to report.
    pub queue_repair: Option<QueueRepair>,
}

pub fn open_state(store: StateStore, loaded: LoadOutcome, clock: &Arc<dyn Clock>) -> OpenedState {
    log_load(&store, &loaded);
    let LoadOutcome {
        state,
        writable,
        queue_repair,
        ..
    } = loaded;
    let sink: Box<dyn StateSink> = if writable {
        Box::new(store)
    } else {
        Box::new(DisabledSink)
    };
    OpenedState {
        session: Session::new(state),
        writer: WriterHandle::spawn(sink, Arc::clone(clock)),
        persisting: writable,
        queue_repair,
    }
}

fn log_load(store: &StateStore, loaded: &LoadOutcome) {
    match &loaded.reason {
        LoadReason::Loaded => tracing::debug!(path = ?store.path(), "state restored"),
        LoadReason::Missing => tracing::debug!(path = ?store.path(), "no state yet"),
        LoadReason::Quarantined { moved_to } => {
            tracing::warn!(
                ?moved_to,
                "state file was unreadable and has been moved aside"
            );
        }
        LoadReason::QuarantineFailed => {
            tracing::warn!("state file is unreadable and could not be moved aside; not writing");
        }
        LoadReason::UnsupportedVersion { found } => {
            tracing::warn!(
                found,
                "state file is from a newer build; preserving it and not writing"
            );
        }
        LoadReason::Unreadable => {
            tracing::warn!("state file could not be read; preserving it and not writing");
        }
    }
    // §6: a repaired queue logs here too, distinct from the warning
    // `StateStore::load` already emits — that one is unconditional, this
    // one is what the TUI's status line surfaces.
    if let Some(repair) = &loaded.queue_repair {
        match &repair.backup {
            QueueBackup::Saved(path) => {
                tracing::warn!(
                    fields = repair.reset.fields_reset(),
                    backup = ?path,
                    "queue data in the state file was reset"
                );
            }
            QueueBackup::Failed => {
                tracing::warn!(
                    fields = repair.reset.fields_reset(),
                    "queue data in the state file was reset; the backup could not be \
                     written, so persistence is disabled for this session"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::runtime::{FlushReport, classify_flush};
    use crate::clock::FakeClock;
    use crate::media::id::{EpisodeKey, FeedId, MediaId};
    use crate::persistence::model::PersistedState;
    use crate::persistence::writer::Urgency;
    use crate::playback::checkpoint::PlaybackCheckpoint;
    use crate::playback::command::ResumeIntent;
    use crate::playback::volume::Volume;
    use crate::resume::ResumeCandidate;
    use std::time::Duration;
    use time::OffsetDateTime;

    fn store_at(path: &std::path::Path) -> (StateStore, Arc<dyn Clock>) {
        let clock: Arc<dyn Clock> = Arc::new(FakeClock::new());
        (
            StateStore::new(path.to_path_buf(), Arc::clone(&clock)),
            clock,
        )
    }

    #[test]
    fn a_state_file_from_a_newer_build_opens_without_writing_and_is_left_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let newer = br#"{"schema_version":99,"current_media":null,"volume":0.5,"checkpoints":{}}"#;
        std::fs::write(&path, newer).unwrap();
        let (store, clock) = store_at(&path);
        let loaded = store.load();

        let mut opened = open_state(store, loaded, &clock);

        assert!(!opened.persisting);
        opened
            .writer
            .submit(PersistedState::default(), Urgency::Forced);
        let outcome = opened.writer.shutdown();
        assert!(
            matches!(
                classify_flush(outcome, opened.persisting),
                FlushReport::Disabled
            ),
            "a session that wrote nothing must not be reported as having written"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            newer,
            "the preserved file must come out byte for byte as it went in"
        );
    }

    #[test]
    fn a_missing_state_file_opens_writable() {
        let dir = tempfile::tempdir().unwrap();
        let (store, clock) = store_at(&dir.path().join("state.json"));
        let loaded = store.load();
        assert!(open_state(store, loaded, &clock).persisting);
    }

    // Moved from `app.rs` with M9.3: what an opened state resumes and
    // restores, read the way the runtime reads it.

    fn episode(guid: &str) -> MediaId {
        MediaId::PodcastEpisode {
            feed: FeedId::new("0123456789abcdef0123456789abcdef".to_string()).unwrap(),
            episode: EpisodeKey::resolve(Some(guid), None, None).unwrap(),
        }
    }

    fn opened_with(stored: &PersistedState) -> (OpenedState, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let (store, clock) = store_at(&dir.path().join("state.json"));
        store.write(stored).unwrap();
        let loaded = store.load();
        (open_state(store, loaded, &clock), dir)
    }

    fn at(position: u64) -> Duration {
        Duration::from_secs(position)
    }

    #[test]
    fn a_stored_entry_becomes_the_resume_candidate_and_the_restored_volume() {
        let media = episode("ep-1");
        let mut stored = PersistedState::default();
        stored.set_volume(Volume::new(0.25));
        stored.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position: at(93),
                updated_at: OffsetDateTime::UNIX_EPOCH,
            },
            false,
        );
        let (opened, _dir) = opened_with(&stored);
        assert_eq!(
            opened.session.resume_intent(&media),
            ResumeIntent::Candidate(ResumeCandidate {
                position: at(93),
                completed: false,
            }),
            "the entry the file held, unresolved — only the worker's probe has a duration"
        );
        assert_eq!(opened.session.state().volume(), Volume::new(0.25));
        assert!(opened.persisting);
    }

    /// §4.3: an estimate and its established fallback resolve to an
    /// `EstimatedCandidate`, preferring the estimate.
    #[test]
    fn a_stored_estimate_and_its_established_fallback_resolve_to_an_estimated_candidate() {
        let media = episode("ep-2");
        let mut stored = PersistedState::default();
        stored.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position: at(40),
                updated_at: OffsetDateTime::UNIX_EPOCH,
            },
            false,
        );
        stored.record_estimated(media.clone(), at(97), OffsetDateTime::UNIX_EPOCH, false);
        let (opened, _dir) = opened_with(&stored);
        assert_eq!(
            opened.session.resume_intent(&media),
            ResumeIntent::EstimatedCandidate {
                target: at(97),
                established: Some(at(40)),
            }
        );
    }

    /// R8: an entry that only ever carried an estimate resolves with
    /// `established: None`, never a fabricated zero.
    #[test]
    fn a_stored_estimate_with_no_established_position_resolves_with_no_fallback() {
        let media = episode("ep-3");
        let mut stored = PersistedState::default();
        stored.record_estimated(media.clone(), at(97), OffsetDateTime::UNIX_EPOCH, false);
        let (opened, _dir) = opened_with(&stored);
        assert_eq!(
            opened.session.resume_intent(&media),
            ResumeIntent::EstimatedCandidate {
                target: at(97),
                established: None,
            }
        );
    }

    /// A completed entry ignores a stray estimate and stays an ordinary
    /// candidate (D1).
    #[test]
    fn a_completed_entry_ignores_a_stray_estimate_and_stays_an_ordinary_candidate() {
        let media = episode("ep-4");
        let mut stored = PersistedState::default();
        stored.record_estimated(media.clone(), at(97), OffsetDateTime::UNIX_EPOCH, true);
        let (opened, _dir) = opened_with(&stored);
        assert_eq!(
            opened.session.resume_intent(&media),
            ResumeIntent::Candidate(ResumeCandidate {
                position: Duration::ZERO,
                completed: true,
            })
        );
    }

    /// The sink selection is the whole feature in one line: this follows a
    /// submitted snapshot all the way to the bytes on disk.
    #[test]
    fn a_submitted_snapshot_reaches_the_state_file_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let (store, clock) = store_at(&path);
        let loaded = store.load();
        let mut opened = open_state(store, loaded, &clock);
        assert!(opened.persisting);

        let media = episode("ep-5");
        let mut advanced = PersistedState::default();
        advanced.set_volume(Volume::new(0.5));
        advanced.record(
            &PlaybackCheckpoint {
                media: media.clone(),
                position: at(150),
                updated_at: OffsetDateTime::UNIX_EPOCH,
            },
            false,
        );
        opened.writer.submit(advanced, Urgency::Forced);
        let outcome = opened.writer.shutdown();
        assert!(matches!(
            classify_flush(outcome, opened.persisting),
            FlushReport::Written
        ));
        let written: PersistedState =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(written.entry_for(&media).unwrap().position, Some(at(150)));
        assert_eq!(written.volume(), Volume::new(0.5));
    }
}
