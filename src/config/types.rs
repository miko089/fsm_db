type PageId = u32;
type SlotId = u16;

struct RowId {
    page_id: PageId,
    slot_id: SlotId,
}
