use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use crate::slotted_page::{NO_NEXT_PAGE, PAGE_SIZE, SlottedPage};
use crate::types::DbError;

pub(crate) type PageId = u32;

/// Manages fixed-size `SlottedPage` persistence on top of a single database file.
pub(crate) struct DiskManager {
    file: File,
    next_page_id: PageId,
}

impl DiskManager {
    /// Opens or creates the database file at `path`, validating page alignment.
    pub(crate) fn open(path: &Path) -> Result<Self, DbError> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;

        let len = file.metadata()?.len();
        if !len.is_multiple_of(PAGE_SIZE as u64) {
            return Err(DbError::StorageError(format!(
                "corrupted database file size {len}: not a multiple of {PAGE_SIZE}"
            )));
        }

        let page_count = len / PAGE_SIZE as u64;
        let next_page_id = PageId::try_from(page_count).map_err(|_| {
            DbError::StorageError(format!(
                "database page count exceeds PageId range: {page_count}"
            ))
        })?;

        Ok(Self { file, next_page_id })
    }

    /// Returns the next page ID that will be assigned by `allocate_page`.
    pub(crate) fn next_page_id(&self) -> PageId {
        self.next_page_id
    }

    /// Reads an allocated page from disk into `page`.
    pub(crate) fn read_page(
        &mut self,
        page_id: PageId,
        page: &mut SlottedPage,
    ) -> Result<(), DbError> {
        if page_id >= self.next_page_id {
            return Err(DbError::StorageError(format!(
                "page {page_id} out of bounds (next_page_id={})",
                self.next_page_id
            )));
        }

        let offset = u64::from(page_id) * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.read_exact(page.as_bytes_mut())?;
        Ok(())
    }

    /// Writes `page` to disk at `page_id`.
    pub(crate) fn write_page(
        &mut self,
        page_id: PageId,
        page: &SlottedPage,
    ) -> Result<(), DbError> {
        if page_id >= self.next_page_id {
            return Err(DbError::StorageError(format!(
                "page {page_id} out of bounds (next_page_id={})",
                self.next_page_id
            )));
        }

        let offset = u64::from(page_id) * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(page.as_bytes())?;
        Ok(())
    }

    /// Allocates a new initialized page at the end of the file and returns its `PageId`.
    pub(crate) fn allocate_page(&mut self) -> Result<PageId, DbError> {
        if self.next_page_id == NO_NEXT_PAGE {
            return Err(DbError::StorageError("page id space exhausted".to_string()));
        }

        let page_id = self.next_page_id;
        let page = SlottedPage::new();
        let offset = u64::from(page_id) * PAGE_SIZE as u64;
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(page.as_bytes())?;
        self.next_page_id += 1;
        Ok(page_id)
    }

    /// Flushes modified file data to the underlying storage device.
    pub(crate) fn sync(&mut self) -> Result<(), DbError> {
        self.file.sync_data()?;
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
    fn allocate_write_reopen_preserves_data_and_next_page_id() {
        let tmp = TempFile::new("disk_persist");

        let (p0, p1) = {
            let mut disk = DiskManager::open(&tmp.path).unwrap();
            assert_eq!(disk.next_page_id(), 0);

            let p0 = disk.allocate_page().unwrap();
            let p1 = disk.allocate_page().unwrap();
            assert_eq!(p0, 0);
            assert_eq!(p1, 1);
            assert_eq!(disk.next_page_id(), 2);

            let mut page0 = SlottedPage::new();
            page0.set_next_page_id(p1);
            assert_eq!(page0.insert(b"hello page zero"), Some(0));
            assert_eq!(page0.insert(b"second slot"), Some(1));
            disk.write_page(p0, &page0).unwrap();

            let mut page1 = SlottedPage::new();
            assert_eq!(page1.insert(b"hello page one"), Some(0));
            disk.write_page(p1, &page1).unwrap();

            disk.sync().unwrap();
            (p0, p1)
        };

        let mut disk = DiskManager::open(&tmp.path).unwrap();
        assert_eq!(disk.next_page_id(), 2);

        let mut read0 = SlottedPage::new();
        disk.read_page(p0, &mut read0).unwrap();
        assert_eq!(read0.next_page_id(), p1);
        assert_eq!(read0.slot_count(), 2);
        assert_eq!(read0.get(0), Some(b"hello page zero".as_slice()));
        assert_eq!(read0.get(1), Some(b"second slot".as_slice()));

        let mut read1 = SlottedPage::new();
        disk.read_page(p1, &mut read1).unwrap();
        assert_eq!(read1.next_page_id(), NO_NEXT_PAGE);
        assert_eq!(read1.slot_count(), 1);
        assert_eq!(read1.get(0), Some(b"hello page one".as_slice()));

        let p2 = disk.allocate_page().unwrap();
        assert_eq!(p2, 2);
        assert_eq!(disk.next_page_id(), 3);
    }

    #[test]
    fn read_and_write_nonexistent_page_return_err() {
        let tmp = TempFile::new("disk_oob");
        let mut disk = DiskManager::open(&tmp.path).unwrap();

        let mut page = SlottedPage::new();
        assert!(matches!(
            disk.read_page(0, &mut page),
            Err(DbError::StorageError(_))
        ));
        assert!(matches!(
            disk.write_page(0, &page),
            Err(DbError::StorageError(_))
        ));

        let p0 = disk.allocate_page().unwrap();
        assert_eq!(p0, 0);
        assert!(matches!(
            disk.read_page(1, &mut page),
            Err(DbError::StorageError(_))
        ));
        assert!(matches!(
            disk.write_page(1, &page),
            Err(DbError::StorageError(_))
        ));
    }

    #[test]
    fn open_unaligned_file_returns_err() {
        let tmp_small = TempFile::new("disk_unaligned_small");
        fs::write(&tmp_small.path, vec![0u8; 100]).unwrap();
        assert!(matches!(
            DiskManager::open(&tmp_small.path),
            Err(DbError::StorageError(_))
        ));

        let tmp_plus_one = TempFile::new("disk_unaligned_plus1");
        fs::write(&tmp_plus_one.path, vec![0u8; PAGE_SIZE + 1]).unwrap();
        assert!(matches!(
            DiskManager::open(&tmp_plus_one.path),
            Err(DbError::StorageError(_))
        ));
    }
}
