pub(crate) const PAGE_SIZE: usize = 4096;
pub(crate) const NO_NEXT_PAGE: u32 = u32::MAX;
const HEADER_SIZE: usize = 8;
const SLOT_SIZE: usize = 4;
pub(crate) const MAX_RECORD_SIZE: usize = PAGE_SIZE - HEADER_SIZE - SLOT_SIZE;

/// 0..4 -> `next_page_id`: `NO_NEXT_PAGE` if last page of a table
/// 4..6 -> `slot_count`: 0 at start
/// 6..8 -> `free_end`: `PAGE_SIZE` at start, grows backwards
struct PageHeader {
    next_page_id: u32,
    slot_count: u16,
    free_end: u16,
}

impl PageHeader {
    fn new() -> Self {
        Self {
            next_page_id: NO_NEXT_PAGE,
            slot_count: 0,
            free_end: PAGE_SIZE as u16,
        }
    }

    fn free_start(&self) -> usize {
        HEADER_SIZE + self.slot_count as usize * SLOT_SIZE
    }

    fn decode(page: &[u8; PAGE_SIZE]) -> Self {
        Self {
            next_page_id: u32::from_le_bytes(page[0..4].try_into().unwrap()),
            slot_count: u16::from_le_bytes(page[4..6].try_into().unwrap()),
            free_end: u16::from_le_bytes(page[6..8].try_into().unwrap()),
        }
    }

    fn encode(&self, page: &mut [u8; PAGE_SIZE]) {
        page[0..4].copy_from_slice(&self.next_page_id.to_le_bytes());
        page[4..6].copy_from_slice(&self.slot_count.to_le_bytes());
        page[6..8].copy_from_slice(&self.free_end.to_le_bytes());
    }
}

struct PageSlot {
    offset: u16,
    len: u16,
}

impl PageSlot {
    const TOMBSTONE: Self = Self { offset: 0, len: 0 };

    fn slot_base(slot_id: u16) -> Option<usize> {
        let base = HEADER_SIZE + slot_id as usize * SLOT_SIZE;
        (base + SLOT_SIZE <= PAGE_SIZE).then_some(base)
    }

    fn is_deleted(&self) -> bool {
        self.len == 0
    }

    fn decode(page: &[u8; PAGE_SIZE], slot_id: u16) -> Option<Self> {
        let base = Self::slot_base(slot_id)?;
        Some(Self {
            offset: u16::from_le_bytes(page[base..base + 2].try_into().unwrap()),
            len: u16::from_le_bytes(page[base + 2..base + 4].try_into().unwrap()),
        })
    }

    fn encode(&self, page: &mut [u8; PAGE_SIZE], slot_id: u16) {
        let base = Self::slot_base(slot_id).expect("slot_id out of page bounds");
        page[base..base + 2].copy_from_slice(&self.offset.to_le_bytes());
        page[base + 2..base + 4].copy_from_slice(&self.len.to_le_bytes());
    }
}

/// First 8 bytes are always `PageHeader`,
/// then `PageSlot`s growing forward and `Row` bytes growing from the end
pub(crate) struct SlottedPage {
    page: [u8; PAGE_SIZE],
}

impl SlottedPage {
    pub(crate) fn new() -> Self {
        let mut p = Self {
            page: [0; PAGE_SIZE],
        };
        p.init();
        p
    }

    pub(crate) fn init(&mut self) {
        self.page.fill(0);
        PageHeader::new().encode(&mut self.page);
    }

    pub(crate) fn as_bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.page
    }

    pub(crate) fn as_bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        &mut self.page
    }

    pub(crate) fn next_page_id(&self) -> u32 {
        let header = PageHeader::decode(&self.page);
        header.next_page_id
    }

    pub(crate) fn set_next_page_id(&mut self, next: u32) {
        let mut header = PageHeader::decode(&self.page);
        header.next_page_id = next;
        header.encode(&mut self.page);
    }

    pub(crate) fn slot_count(&self) -> u16 {
        let header = PageHeader::decode(&self.page);
        header.slot_count
    }

    fn compact(&mut self) {
        let mut header = PageHeader::decode(&self.page);
        let mut tmp = [0u8; PAGE_SIZE];
        let mut write_end = PAGE_SIZE;

        for slot_id in 0..header.slot_count {
            let mut slot =
                PageSlot::decode(&self.page, slot_id).expect("slot_id out of page bounds");
            if slot.is_deleted() {
                continue;
            }
            let start = slot.offset as usize;
            let len = slot.len as usize;
            write_end -= len;
            tmp[write_end..write_end + len].copy_from_slice(&self.page[start..start + len]);
            slot.offset = write_end as u16;
            slot.encode(&mut self.page, slot_id);
        }

        self.page[write_end..PAGE_SIZE].copy_from_slice(&tmp[write_end..PAGE_SIZE]);
        self.page[header.free_start()..write_end].fill(0);
        header.free_end = write_end as u16;
        header.encode(&mut self.page);
    }

    pub(crate) fn insert(&mut self, data: &[u8]) -> Option<u16> {
        if data.is_empty() {
            return None;
        }
        let mut header = PageHeader::decode(&self.page);
        let reusable_slot = (0..header.slot_count).find(|&slot_id| {
            PageSlot::decode(&self.page, slot_id).is_some_and(|slot| slot.is_deleted())
        });

        let slot_overhead = if reusable_slot.is_some() {
            0
        } else {
            SLOT_SIZE
        };
        let needed = data.len().checked_add(slot_overhead)?;

        let contiguous_free = (header.free_end as usize).saturating_sub(header.free_start());
        if contiguous_free < needed {
            self.compact();
            header = PageHeader::decode(&self.page);
            let contiguous_after_compact =
                (header.free_end as usize).saturating_sub(header.free_start());
            if contiguous_after_compact < needed {
                return None;
            }
        }

        let old_free_end = header.free_end as usize;
        let new_free_end = old_free_end - data.len();
        self.page[new_free_end..old_free_end].copy_from_slice(data);

        let slot_id = match reusable_slot {
            Some(id) => id,
            None => {
                let id = header.slot_count;
                header.slot_count += 1;
                id
            }
        };

        header.free_end = new_free_end as u16;
        let slot = PageSlot {
            offset: header.free_end,
            len: data.len() as u16,
        };
        slot.encode(&mut self.page, slot_id);
        header.encode(&mut self.page);

        Some(slot_id)
    }

    pub(crate) fn get(&self, slot_id: u16) -> Option<&[u8]> {
        let header = PageHeader::decode(&self.page);
        if slot_id >= header.slot_count {
            return None;
        }
        let slot = PageSlot::decode(&self.page, slot_id)?;
        if slot.is_deleted() {
            return None;
        }
        let start = slot.offset as usize;
        let end = start.checked_add(slot.len as usize)?;
        self.page.get(start..end)
    }

    pub(crate) fn delete(&mut self, slot_id: u16) -> bool {
        let header = PageHeader::decode(&self.page);
        if slot_id >= header.slot_count {
            return false;
        }
        let Some(slot) = PageSlot::decode(&self.page, slot_id) else {
            return false;
        };
        if slot.is_deleted() {
            return false;
        }

        PageSlot::TOMBSTONE.encode(&mut self.page, slot_id);
        true
    }

    pub(crate) fn update_in_place(&mut self, slot_id: u16, data: &[u8]) -> bool {
        if data.is_empty() {
            return false;
        }

        let mut header = PageHeader::decode(&self.page);
        if slot_id >= header.slot_count {
            return false;
        }
        let Some(mut slot) = PageSlot::decode(&self.page, slot_id) else {
            return false;
        };
        if slot.is_deleted() {
            return false;
        }

        let old_len = slot.len as usize;
        let new_len = data.len();

        if new_len <= old_len {
            let start = slot.offset as usize;
            self.page[start..start + new_len].copy_from_slice(data);
            if new_len < old_len {
                self.page[start + new_len..start + old_len].fill(0);
            }
            slot.len = new_len as u16;
            slot.encode(&mut self.page, slot_id);
            return true;
        }

        let contiguous_free = (header.free_end as usize).saturating_sub(header.free_start());
        if contiguous_free >= new_len {
            let new_free_end = header.free_end as usize - new_len;
            self.page[new_free_end..header.free_end as usize].copy_from_slice(data);
            let old_start = slot.offset as usize;
            self.page[old_start..old_start + old_len].fill(0);

            header.free_end = new_free_end as u16;
            slot.offset = header.free_end;
            slot.len = new_len as u16;
            slot.encode(&mut self.page, slot_id);
            header.encode(&mut self.page);
            return true;
        }

        let other_live_bytes: usize = (0..header.slot_count)
            .filter(|&id| id != slot_id)
            .filter_map(|id| PageSlot::decode(&self.page, id))
            .filter(|s| !s.is_deleted())
            .map(|s| s.len as usize)
            .sum();

        let max_available = PAGE_SIZE - header.free_start() - other_live_bytes;
        if new_len > max_available {
            return false;
        }

        PageSlot::TOMBSTONE.encode(&mut self.page, slot_id);
        self.compact();
        header = PageHeader::decode(&self.page);

        let new_free_end = header.free_end as usize - new_len;
        self.page[new_free_end..header.free_end as usize].copy_from_slice(data);

        header.free_end = new_free_end as u16;
        let updated_slot = PageSlot {
            offset: header.free_end,
            len: new_len as u16,
        };
        updated_slot.encode(&mut self.page, slot_id);
        header.encode(&mut self.page);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_slot_bytes(page: &SlottedPage, slot_id: u16) -> Option<&[u8]> {
        let header = PageHeader::decode(page.as_bytes());
        if slot_id >= header.slot_count {
            return None;
        }
        let slot = PageSlot::decode(page.as_bytes(), slot_id)?;
        if slot.is_deleted() {
            return None;
        }
        let start = slot.offset as usize;
        let end = start + slot.len as usize;
        Some(&page.as_bytes()[start..end])
    }

    fn tombstone_slot(page: &mut SlottedPage, slot_id: u16) {
        PageSlot::TOMBSTONE.encode(page.as_bytes_mut(), slot_id);
    }

    #[test]
    fn insert_basic_and_bounds() {
        let mut sp = SlottedPage::new();

        assert_eq!(sp.insert(&[]), None);
        assert_eq!(
            sp.insert(&vec![1u8; PAGE_SIZE - HEADER_SIZE - SLOT_SIZE + 1]),
            None
        );

        let s0 = sp.insert(b"hello").unwrap();
        let s1 = sp.insert(b"world!").unwrap();
        assert_eq!(s0, 0);
        assert_eq!(s1, 1);
        assert_eq!(sp.slot_count(), 2);
        assert_eq!(read_slot_bytes(&sp, s0), Some(b"hello".as_slice()));
        assert_eq!(read_slot_bytes(&sp, s1), Some(b"world!".as_slice()));
    }

    #[test]
    fn insert_exact_fit_and_full_page() {
        let mut sp = SlottedPage::new();
        let max_len = PAGE_SIZE - HEADER_SIZE - SLOT_SIZE;
        let payload = vec![0xAB; max_len];

        assert_eq!(sp.insert(&payload), Some(0));
        assert_eq!(read_slot_bytes(&sp, 0), Some(payload.as_slice()));
        assert_eq!(sp.insert(b"x"), None);
    }

    #[test]
    fn insert_triggers_compact_and_reuses_tombstone() {
        let mut sp = SlottedPage::new();

        // Fill most of the page with 4 records of 1000 bytes each (4 * 1004 = 4016 bytes;
        // 4096 - 8 - 4016 = 72 bytes of contiguous free space left).
        let r0 = vec![0u8; 1000];
        let r1 = vec![1u8; 1000];
        let r2 = vec![2u8; 1000];
        let r3 = vec![3u8; 1000];

        assert_eq!(sp.insert(&r0), Some(0));
        assert_eq!(sp.insert(&r1), Some(1));
        assert_eq!(sp.insert(&r2), Some(2));
        assert_eq!(sp.insert(&r3), Some(3));

        // Tombstone slot 1 (1000 bytes fragmented in the middle).
        // Contiguous free space is still 72 bytes, so a 1050-byte insert must compact.
        tombstone_slot(&mut sp, 1);

        let r_new = vec![9u8; 1050];
        let new_slot = sp.insert(&r_new).unwrap();
        assert_eq!(new_slot, 1);
        assert_eq!(sp.slot_count(), 4);

        // Live slots keep their IDs and exact contents.
        assert_eq!(read_slot_bytes(&sp, 0), Some(r0.as_slice()));
        assert_eq!(read_slot_bytes(&sp, 1), Some(r_new.as_slice()));
        assert_eq!(read_slot_bytes(&sp, 2), Some(r2.as_slice()));
        assert_eq!(read_slot_bytes(&sp, 3), Some(r3.as_slice()));
    }

    #[test]
    fn insert_compacts_multiple_fragments_and_appends_when_no_tombstone_left() {
        let mut sp = SlottedPage::new();
        let a = vec![10u8; 1500];
        let b = vec![20u8; 1500];
        let c = vec![30u8; 1000];

        assert_eq!(sp.insert(&a), Some(0));
        assert_eq!(sp.insert(&b), Some(1));
        assert_eq!(sp.insert(&c), Some(2));

        // Free space = 4096 - 8 - 12 - 4000 = 76 bytes.
        // Tombstone slot 0 (1500 bytes), then insert 1400 bytes -> reuses slot 0 after compact.
        tombstone_slot(&mut sp, 0);
        let d = vec![40u8; 1400];
        assert_eq!(sp.insert(&d), Some(0));
        assert_eq!(sp.slot_count(), 3);

        // Now free space is 76 + (1500 - 1400) = 176 bytes.
        // No tombstones remain; inserting 170 bytes (+ 4 byte slot = 174) should succeed at slot 3.
        let e = vec![50u8; 170];
        assert_eq!(sp.insert(&e), Some(3));
        assert_eq!(sp.slot_count(), 4);

        assert_eq!(read_slot_bytes(&sp, 0), Some(d.as_slice()));
        assert_eq!(read_slot_bytes(&sp, 1), Some(b.as_slice()));
        assert_eq!(read_slot_bytes(&sp, 2), Some(c.as_slice()));
        assert_eq!(read_slot_bytes(&sp, 3), Some(e.as_slice()));
    }

    #[test]
    fn delete_basic_bounds_and_reuse() {
        let mut sp = SlottedPage::new();
        assert!(!sp.delete(0));
        assert!(!sp.delete(u16::MAX));

        let s0 = sp.insert(b"alpha").unwrap();
        let s1 = sp.insert(b"beta").unwrap();
        assert!(sp.delete(s0));
        assert!(!sp.delete(s0));
        assert_eq!(read_slot_bytes(&sp, s0), None);
        assert_eq!(read_slot_bytes(&sp, s1), Some(b"beta".as_slice()));
        assert!(!sp.delete(2));

        let reused = sp.insert(b"gamma").unwrap();
        assert_eq!(reused, s0);
        assert_eq!(read_slot_bytes(&sp, reused), Some(b"gamma".as_slice()));
    }

    #[test]
    fn update_in_place_same_and_smaller_size() {
        let mut sp = SlottedPage::new();
        let s0 = sp.insert(b"hello world").unwrap();
        let s1 = sp.insert(b"second").unwrap();

        assert!(sp.update_in_place(s0, b"HELLO WORLD"));
        assert_eq!(read_slot_bytes(&sp, s0), Some(b"HELLO WORLD".as_slice()));

        assert!(sp.update_in_place(s0, b"hi"));
        assert_eq!(read_slot_bytes(&sp, s0), Some(b"hi".as_slice()));
        assert_eq!(read_slot_bytes(&sp, s1), Some(b"second".as_slice()));
    }

    #[test]
    fn update_in_place_larger_with_free_space_and_with_compaction() {
        let mut sp = SlottedPage::new();
        let r0 = vec![1u8; 1500];
        let r1 = vec![2u8; 1500];
        let r2 = vec![3u8; 900];
        assert_eq!(sp.insert(&r0), Some(0));
        assert_eq!(sp.insert(&r1), Some(1));
        assert_eq!(sp.insert(&r2), Some(2));

        // Contiguous free space: 4096 - 8 - 12 - 3900 = 176 bytes.
        // Grow slot 2 from 900 to 950 -> needs compaction (reclaiming slot 2's 900 bytes + 176 = 1076).
        let r2_grown = vec![4u8; 950];
        assert!(sp.update_in_place(2, &r2_grown));
        assert_eq!(read_slot_bytes(&sp, 0), Some(r0.as_slice()));
        assert_eq!(read_slot_bytes(&sp, 1), Some(r1.as_slice()));
        assert_eq!(read_slot_bytes(&sp, 2), Some(r2_grown.as_slice()));

        // Delete slot 0 (1500 bytes), then grow slot 2 to 2000 bytes -> compacts and succeeds.
        assert!(sp.delete(0));
        let r2_big = vec![5u8; 2000];
        assert!(sp.update_in_place(2, &r2_big));
        assert_eq!(read_slot_bytes(&sp, 0), None);
        assert_eq!(read_slot_bytes(&sp, 1), Some(r1.as_slice()));
        assert_eq!(read_slot_bytes(&sp, 2), Some(r2_big.as_slice()));
    }

    #[test]
    fn update_in_place_invalid_and_overflow_preserves_old_data() {
        let mut sp = SlottedPage::new();
        assert!(!sp.update_in_place(0, b"x"));

        let r0 = vec![7u8; 2000];
        let r1 = vec![8u8; 2000];
        assert_eq!(sp.insert(&r0), Some(0));
        assert_eq!(sp.insert(&r1), Some(1));

        assert!(!sp.update_in_place(0, &[]));
        assert!(!sp.update_in_place(2, b"x"));

        // Max available for slot 0 is 4096 - 8 - 8 - 2000 = 2080 bytes.
        let too_big = vec![9u8; 2081];
        assert!(!sp.update_in_place(0, &too_big));
        assert_eq!(read_slot_bytes(&sp, 0), Some(r0.as_slice()));
        assert_eq!(read_slot_bytes(&sp, 1), Some(r1.as_slice()));

        assert!(sp.delete(0));
        assert!(!sp.update_in_place(0, b"revive"));
    }

    #[test]
    fn get_returns_none_for_out_of_bounds_and_deleted_slots() {
        let mut sp = SlottedPage::new();
        assert_eq!(sp.get(0), None);
        assert_eq!(sp.get(u16::MAX), None);

        let s0 = sp.insert(b"alpha").unwrap();
        let s1 = sp.insert(b"beta").unwrap();
        assert_eq!(sp.get(s0), Some(b"alpha".as_slice()));
        assert_eq!(sp.get(s1), Some(b"beta".as_slice()));
        assert_eq!(sp.get(2), None);

        assert!(sp.delete(s0));
        assert_eq!(sp.get(s0), None);
        assert_eq!(sp.get(s1), Some(b"beta".as_slice()));
    }
}
