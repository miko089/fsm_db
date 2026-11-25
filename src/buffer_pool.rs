use std::collections::HashMap;

use crate::disk::{DiskManager, PageId};
use crate::slotted_page::SlottedPage;
use crate::types::DbError;

struct Frame {
    page_id: Option<PageId>,
    page: SlottedPage,
    is_dirty: bool,
    pin_count: usize,
}

impl Frame {
    fn empty() -> Self {
        Self {
            page_id: None,
            page: SlottedPage::new(),
            is_dirty: false,
            pin_count: 0,
        }
    }
}

/// Fixed-capacity in-memory page cache backed by `DiskManager`.
pub(crate) struct BufferPool {
    disk: DiskManager,
    frames: Vec<Frame>,
    page_table: HashMap<PageId, usize>,
    evict_cursor: usize,
}

impl BufferPool {
    /// Creates a `BufferPool` with `capacity` frames (`capacity > 0`).
    pub(crate) fn new(disk: DiskManager, capacity: usize) -> Self {
        assert!(capacity > 0, "buffer pool capacity must be > 0");
        let frames = (0..capacity).map(|_| Frame::empty()).collect();
        Self {
            disk,
            frames,
            page_table: HashMap::with_capacity(capacity),
            evict_cursor: 0,
        }
    }

    fn acquire_frame(&mut self) -> Result<usize, DbError> {
        if let Some(idx) = self.frames.iter().position(|f| f.page_id.is_none()) {
            return Ok(idx);
        }

        let len = self.frames.len();
        for step in 0..len {
            let idx = (self.evict_cursor + step) % len;
            if self.frames[idx].pin_count == 0 {
                self.evict_cursor = (idx + 1) % len;
                if let Some(old_page_id) = self.frames[idx].page_id {
                    if self.frames[idx].is_dirty {
                        self.disk.write_page(old_page_id, &self.frames[idx].page)?;
                        self.frames[idx].is_dirty = false;
                    }
                    self.page_table.remove(&old_page_id);
                    self.frames[idx].page_id = None;
                }
                return Ok(idx);
            }
        }

        Err(DbError::StorageError(
            "all buffer pool frames are pinned".to_string(),
        ))
    }

    fn fetch_frame_idx(&mut self, page_id: PageId) -> Result<usize, DbError> {
        if let Some(&frame_idx) = self.page_table.get(&page_id) {
            self.frames[frame_idx].pin_count += 1;
            return Ok(frame_idx);
        }

        let frame_idx = self.acquire_frame()?;
        self.disk
            .read_page(page_id, &mut self.frames[frame_idx].page)?;

        let frame = &mut self.frames[frame_idx];
        frame.page_id = Some(page_id);
        frame.pin_count = 1;
        frame.is_dirty = false;
        self.page_table.insert(page_id, frame_idx);
        Ok(frame_idx)
    }

    /// Pins and returns a shared reference to the requested page.
    pub(crate) fn fetch_page(&mut self, page_id: PageId) -> Result<&SlottedPage, DbError> {
        let frame_idx = self.fetch_frame_idx(page_id)?;
        Ok(&self.frames[frame_idx].page)
    }

    /// Pins and returns a mutable reference to the requested page.
    pub(crate) fn fetch_page_mut(&mut self, page_id: PageId) -> Result<&mut SlottedPage, DbError> {
        let frame_idx = self.fetch_frame_idx(page_id)?;
        Ok(&mut self.frames[frame_idx].page)
    }

    /// Allocates a new page on disk, pins it in a frame, and returns its ID and buffer.
    pub(crate) fn new_page(&mut self) -> Result<(PageId, &mut SlottedPage), DbError> {
        let frame_idx = self.acquire_frame()?;
        let page_id = self.disk.allocate_page()?;
        self.page_table.insert(page_id, frame_idx);

        let frame = &mut self.frames[frame_idx];
        frame.page.init();
        frame.page_id = Some(page_id);
        frame.pin_count = 1;
        frame.is_dirty = true;
        Ok((page_id, &mut frame.page))
    }

    /// Decrements the pin count for `page_id` and records whether it was dirtied.
    pub(crate) fn unpin_page(&mut self, page_id: PageId, is_dirty: bool) -> Result<(), DbError> {
        let Some(&frame_idx) = self.page_table.get(&page_id) else {
            return Err(DbError::StorageError(format!(
                "page {page_id} is not in the buffer pool"
            )));
        };

        let frame = &mut self.frames[frame_idx];
        if frame.pin_count == 0 {
            return Err(DbError::StorageError(format!(
                "page {page_id} already has pin_count == 0"
            )));
        }

        frame.pin_count -= 1;
        frame.is_dirty |= is_dirty;
        Ok(())
    }

    /// Writes all dirty frames to disk and syncs the underlying file.
    pub(crate) fn flush_all(&mut self) -> Result<(), DbError> {
        for frame in &mut self.frames {
            if let Some(page_id) = frame.page_id
                && frame.is_dirty
            {
                self.disk.write_page(page_id, &frame.page)?;
                frame.is_dirty = false;
            }
        }
        self.disk.sync()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use super::*;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

    struct TempFile {
        path: PathBuf,
    }

    impl TempFile {
        fn new(prefix: &str) -> Self {
            let id = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("fsm_db_{prefix}_{}_{id}.db", std::process::id()));
            let _ = fs::remove_file(&path);
            Self { path }
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    #[test]
    fn overflow_eviction_and_reopen_persist_all_pages() {
        let tmp = TempFile::new("bp_evict");
        let payloads = [
            b"page-0-alpha".as_slice(),
            b"page-1-beta".as_slice(),
            b"page-2-gamma".as_slice(),
            b"page-3-delta".as_slice(),
        ];

        let mut page_ids = Vec::new();
        {
            let disk = DiskManager::open(&tmp.path).unwrap();
            let mut pool = BufferPool::new(disk, 2);

            for payload in payloads {
                let (pid, page) = pool.new_page().unwrap();
                assert_eq!(page.insert(payload), Some(0));
                page_ids.push(pid);
                pool.unpin_page(pid, true).unwrap();
            }

            // Re-fetch every page in the same pool (forcing evictions and disk reads).
            for (&pid, &expected) in page_ids.iter().zip(payloads.iter()) {
                let page = pool.fetch_page(pid).unwrap();
                assert_eq!(page.get(0), Some(expected));
                pool.unpin_page(pid, false).unwrap();
            }

            pool.flush_all().unwrap();
        }

        // Reopen on a fresh DiskManager + BufferPool and verify all 4 pages.
        let disk = DiskManager::open(&tmp.path).unwrap();
        assert_eq!(disk.next_page_id(), 4);
        let mut pool = BufferPool::new(disk, 2);
        for (&pid, &expected) in page_ids.iter().zip(payloads.iter()) {
            let page = pool.fetch_page(pid).unwrap();
            assert_eq!(page.get(0), Some(expected));
            pool.unpin_page(pid, false).unwrap();
        }
    }

    #[test]
    fn sticky_dirty_flag_survives_clean_unpin_and_eviction() {
        let tmp = TempFile::new("bp_sticky_dirty");
        let disk = DiskManager::open(&tmp.path).unwrap();
        let mut pool = BufferPool::new(disk, 2);

        let p0 = {
            let (pid, page) = pool.new_page().unwrap();
            assert_eq!(page.insert(b"initial"), Some(0));
            pool.unpin_page(pid, true).unwrap();
            pid
        };
        pool.flush_all().unwrap();

        // Modify page 0 and unpin with is_dirty = true.
        {
            let page = pool.fetch_page_mut(p0).unwrap();
            assert!(page.update_in_place(0, b"modified"));
            pool.unpin_page(p0, true).unwrap();
        }

        // Re-fetch page 0 and unpin with is_dirty = false; dirty flag must remain true.
        {
            let page = pool.fetch_page(p0).unwrap();
            assert_eq!(page.get(0), Some(b"modified".as_slice()));
            pool.unpin_page(p0, false).unwrap();
        }

        // Evict page 0 by allocating and unpinning two other pages (capacity = 2).
        let (p1, _) = pool.new_page().unwrap();
        pool.unpin_page(p1, true).unwrap();
        let (p2, _) = pool.new_page().unwrap();
        pool.unpin_page(p2, true).unwrap();

        // Fetching page 0 back from disk must show the modification.
        let page = pool.fetch_page(p0).unwrap();
        assert_eq!(page.get(0), Some(b"modified".as_slice()));
        pool.unpin_page(p0, false).unwrap();
    }

    #[test]
    fn all_frames_pinned_returns_error_without_leaking_pages() {
        let tmp = TempFile::new("bp_all_pinned");
        let disk = DiskManager::open(&tmp.path).unwrap();
        let mut pool = BufferPool::new(disk, 2);

        let (p0, _) = pool.new_page().unwrap();
        pool.unpin_page(p0, true).unwrap();
        let (p1, _) = pool.new_page().unwrap();
        pool.unpin_page(p1, true).unwrap();
        let (p2, _) = pool.new_page().unwrap();
        pool.unpin_page(p2, true).unwrap();

        // Pin 2 pages in the capacity-2 pool.
        assert!(pool.fetch_page(p0).is_ok());
        assert!(pool.fetch_page(p1).is_ok());

        // Pool is full and all frames are pinned: new_page and fetch_page(p2) must fail.
        assert!(matches!(pool.new_page(), Err(DbError::StorageError(_))));
        assert!(matches!(pool.fetch_page(p2), Err(DbError::StorageError(_))));
        assert!(matches!(
            pool.fetch_page_mut(p2),
            Err(DbError::StorageError(_))
        ));

        // Unpin one frame and verify no disk page was leaked by the failed new_page().
        pool.unpin_page(p0, false).unwrap();
        let (p3, _) = pool.new_page().unwrap();
        assert_eq!(p3, 3);
        pool.unpin_page(p3, false).unwrap();
        pool.unpin_page(p1, false).unwrap();

        // Verify unpin error cases.
        assert!(matches!(
            pool.unpin_page(p1, false),
            Err(DbError::StorageError(_))
        ));
        assert!(matches!(
            pool.unpin_page(999, false),
            Err(DbError::StorageError(_))
        ));
    }
}
