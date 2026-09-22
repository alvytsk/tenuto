#[cfg(test)]
mod tests {
    use tenuto::media::id::{AbsolutePath, MediaId};
    use tenuto::queue::{
        Direction, DisplayMetadata, IdAllocator, NewQueueEntry, Queue, QueueError, QueueSource,
    };

    fn local(name: &str) -> NewQueueEntry {
        let path = AbsolutePath::new(format!("/music/{name}.flac").into()).expect("absolute");
        NewQueueEntry::new(
            MediaId::LocalFile(path.clone()),
            QueueSource::LocalFile(path),
            DisplayMetadata::default(),
        )
        .expect("matching source")
    }

    fn many(prefix: &str, count: usize) -> Vec<NewQueueEntry> {
        (0..count).map(|i| local(&format!("{prefix}{i}"))).collect()
    }

    #[test]
    fn duplicate_media_gets_distinct_occurrence_ids() {
        let mut queue = Queue::default();
        let ids = queue
            .enqueue(vec![local("a"), local("a")], &mut IdAllocator::default())
            .expect("fits");
        assert_ne!(ids[0], ids[1]);
        assert_eq!(
            queue.get(ids[0]).map(|e| e.media()),
            queue.get(ids[1]).map(|e| e.media())
        );
    }

    #[test]
    fn a_mismatched_source_is_refused_at_construction() {
        let a = AbsolutePath::new("/music/a.flac".into()).expect("absolute");
        let b = AbsolutePath::new("/music/b.flac".into()).expect("absolute");
        let result = NewQueueEntry::new(
            MediaId::LocalFile(a),
            QueueSource::LocalFile(b),
            DisplayMetadata::default(),
        );
        assert!(matches!(result, Err(QueueError::SourceMismatch)));
    }

    #[test]
    fn reordering_keeps_ids_and_stops_at_the_edges() {
        let mut queue = Queue::default();
        let mut ids_alloc = IdAllocator::default();
        let ids = queue.enqueue(many("r", 3), &mut ids_alloc).expect("fits");
        assert!(queue.move_entry(ids[2], Direction::Up).expect("known"));
        assert_eq!(
            queue.entries().iter().map(|e| e.id()).collect::<Vec<_>>(),
            [ids[0], ids[2], ids[1]]
        );
        assert!(!queue.move_entry(ids[0], Direction::Up).expect("known"));
        let unknown_id = tenuto_unknown_id(&mut queue, &mut ids_alloc);
        assert!(matches!(
            queue.move_entry(unknown_id, Direction::Down),
            Err(QueueError::UnknownEntry(_))
        ));
    }

    /// An ID that no longer exists: enqueue then remove it.
    fn tenuto_unknown_id(queue: &mut Queue, ids: &mut IdAllocator) -> tenuto::queue::QueueEntryId {
        let id = queue.enqueue(vec![local("gone")], ids).expect("fits")[0];
        queue.remove(id).expect("known");
        id
    }

    #[test]
    fn removal_selects_the_successor_or_the_predecessor_at_the_end() {
        let mut queue = Queue::default();
        let ids = queue
            .enqueue(many("s", 3), &mut IdAllocator::default())
            .expect("fits");
        assert_eq!(queue.remove(ids[1]).expect("known").selection, Some(ids[2]));
        assert_eq!(queue.remove(ids[2]).expect("known").selection, Some(ids[0]));
        assert_eq!(queue.remove(ids[0]).expect("known").selection, None);
    }

    #[test]
    fn ids_are_never_reused_after_removal_or_clear() {
        let mut queue = Queue::default();
        let mut ids_alloc = IdAllocator::default();
        let first = queue
            .enqueue(vec![local("a")], &mut ids_alloc)
            .expect("fits")[0];
        queue.clear();
        let second = queue
            .enqueue(vec![local("a")], &mut ids_alloc)
            .expect("fits")[0];
        assert!(second > first);
    }

    #[test]
    fn neighbors_do_not_wrap() {
        let mut queue = Queue::default();
        let ids = queue
            .enqueue(many("n", 2), &mut IdAllocator::default())
            .expect("fits");
        assert_eq!(queue.neighbor(ids[0], Direction::Down), Some(ids[1]));
        assert_eq!(queue.neighbor(ids[1], Direction::Down), None);
        assert_eq!(queue.neighbor(ids[0], Direction::Up), None);
    }
}
