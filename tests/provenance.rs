use tenuto::media::provenance::PositionProvenance;
use tenuto::playback::timeline::PositionQuality;

#[test]
fn provenance_and_quality_are_independent_axes() {
    // The whole point of a second field. Every combination is meaningful, so
    // none of them may be collapsed into the other enum.
    for quality in [
        PositionQuality::Exact,
        PositionQuality::Estimated,
        PositionQuality::Degraded,
    ] {
        for provenance in [
            PositionProvenance::Established,
            PositionProvenance::Estimated,
        ] {
            // Constructing the pair is the assertion: if a later change folds
            // provenance into quality this stops compiling.
            let _ = (quality, provenance);
        }
    }
}

// A degraded-quality-but-established-provenance test used to live here as
// two self-asserting literals, which could not fail against any
// implementation, including a broken one. It exercises the real publish
// path instead now: `tests/wait_service.rs::
// a_degraded_quality_does_not_imply_estimated_provenance` — that file
// already has the `SessionFacts`/`WaitService` harness this needs, which
// this file does not.

#[test]
fn established_is_the_default_so_every_existing_path_keeps_its_meaning() {
    // M1 and M2 wrote positions the decoder confirmed. Anything that does not
    // opt into an estimate must keep reporting what it always reported.
    assert_eq!(
        PositionProvenance::default(),
        PositionProvenance::Established
    );
}
