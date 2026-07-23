//! Server → client Window List orders (MS-RDPERP §2.2.1.3).
//!
//! Unlike the RAIL control PDUs (which ride the `rail` static channel), these
//! are **alternate-secondary drawing orders** carried in the graphics update
//! stream (a fast-path `Orders` update). They tell the client about the
//! server's windows: create/update/delete, icons, taskbar notify icons, and the
//! monitored-desktop / z-order.
//!
//! Each window order is self-describing via an `orderSize` field, so this parser
//! walks the order list order-by-order and bounds every field read to the
//! current order — a malformed or not-yet-modeled field can never read past the
//! order boundary. Non-window orders in the stream (frame markers, etc.) are not
//! decoded; parsing stops at the first order that isn't an ALTSEC window order,
//! returning whatever windows were decoded so far.

use bitflags::bitflags;
use ironrdp_core::ReadCursor;

/// `TS_STANDARD` in a drawing-order `controlFlags` (MS-RDPEGDI §2.2.2.2.1.1.2).
const CONTROL_FLAG_STANDARD: u8 = 0x01;
/// `TS_SECONDARY`.
const CONTROL_FLAG_SECONDARY: u8 = 0x02;
/// The alternate-secondary order type carrying window information.
const ALTSEC_WINDOW: u8 = 0x0B;

bitflags! {
    /// `FieldsPresentFlags` of a window/notify/desktop order (MS-RDPERP §2.2.1.3.1.1).
    ///
    /// The high bits select the order *class* and *state*; the low bits mark which
    /// optional fields are present in a New-or-Existing Window order.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct WindowFieldFlags: u32 {
        // Field-present bits (New or Existing Window order, §2.2.1.3.1.2.1).
        const OWNER = 0x0000_0002;
        const STYLE = 0x0000_0008;
        const SHOW = 0x0000_0010;
        const TITLE = 0x0000_0004;
        const CLIENT_AREA_OFFSET = 0x0000_4000;
        const CLIENT_AREA_SIZE = 0x0001_0000;
        const RP_CONTENT = 0x0002_0000;
        const ROOT_PARENT = 0x0004_0000;
        const WND_OFFSET = 0x0000_0800;
        const WND_CLIENT_DELTA = 0x0000_8000;
        const WND_SIZE = 0x0000_0400;
        const WND_RECTS = 0x0000_0100;
        const VIS_OFFSET = 0x0000_1000;
        const VISIBILITY = 0x0000_0200;
        // Class / state bits.
        const TYPE_WINDOW = 0x0100_0000;
        const TYPE_NOTIFY = 0x0200_0000;
        const TYPE_DESKTOP = 0x0400_0000;
        const STATE_NEW = 0x1000_0000;
        const STATE_DELETED = 0x2000_0000;
        const ICON = 0x4000_0000;
        const CACHED_ICON = 0x8000_0000;
    }
}

/// A point (`INT32` pair) from a window order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Point {
    pub x: i32,
    pub y: i32,
}

/// A size (`INT32` pair) from a window order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Size {
    pub width: i32,
    pub height: i32,
}

/// The geometry/state of a window, as decoded from a New-or-Existing Window
/// order. Every field is optional: a window *update* only carries the fields
/// whose bit is set in `field_flags`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowState {
    pub owner_window_id: Option<u32>,
    pub style: Option<u32>,
    pub extended_style: Option<u32>,
    pub show_state: Option<u8>,
    pub title: Option<String>,
    pub client_offset: Option<Point>,
    pub client_area_size: Option<Size>,
    pub window_offset: Option<Point>,
    pub window_client_delta: Option<Point>,
    pub window_size: Option<Size>,
    pub visible_offset: Option<Point>,
}

/// A decoded Window List order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WindowOrder {
    /// A new window (`WINDOW_ORDER_STATE_NEW`).
    CreateWindow { window_id: u32, state: WindowState },
    /// An update to an existing window.
    UpdateWindow { window_id: u32, state: WindowState },
    /// The window was destroyed (`WINDOW_ORDER_STATE_DELETED`).
    DeleteWindow { window_id: u32 },
    /// A window icon order (big/small); the icon payload is not decoded here.
    WindowIcon { window_id: u32, cached: bool },
    /// A taskbar notification (tray) icon order.
    NotifyIcon {
        window_id: u32,
        notify_id: u32,
        deleted: bool,
    },
    /// A monitored-desktop / z-order order (or non-monitored when `window_ids`
    /// is empty and `active_window_id` is `None`).
    Desktop {
        active_window_id: Option<u32>,
        window_ids: Vec<u32>,
    },
}

impl WindowOrder {
    /// Decode all ALTSEC window orders from a fast-path `Orders` update body.
    ///
    /// `body` is the update data *after* the fast-path update header, i.e.
    /// `numberOrders` (u16) followed by the order list. Parsing stops at the
    /// first non-window order (returning the windows decoded so far), because
    /// primary/other orders are not self-describing enough to skip reliably.
    pub fn decode_orders_update(body: &[u8]) -> Vec<WindowOrder> {
        let mut cursor = ReadCursor::new(body);
        if cursor.len() < 2 {
            return Vec::new();
        }
        let number_orders = cursor.read_u16();

        let mut orders = Vec::new();
        for _ in 0..number_orders {
            match Self::decode_one(&mut cursor) {
                Some(order) => orders.push(order),
                None => break,
            }
        }
        orders
    }

    /// Decode a single order; returns `None` (stopping the walk) for anything
    /// that isn't an ALTSEC window order or that would read out of bounds.
    fn decode_one(cursor: &mut ReadCursor<'_>) -> Option<WindowOrder> {
        if cursor.is_empty() {
            return None;
        }
        let start = cursor.len(); // remaining bytes at the controlFlags byte
        let control_flags = cursor.read_u8();

        // Only alternate-secondary orders (TS_STANDARD clear) of type WINDOW are
        // handled; everything else stops the walk.
        if control_flags & CONTROL_FLAG_STANDARD != 0 || control_flags & CONTROL_FLAG_SECONDARY != 0 {
            return None;
        }
        let order_type = control_flags >> 2;
        if order_type != ALTSEC_WINDOW {
            return None;
        }

        // Header: OrderSize (measured from the controlFlags byte) + FieldsPresentFlags.
        if cursor.len() < 6 {
            return None;
        }
        let order_size = usize::from(cursor.read_u16());
        let field_flags = WindowFieldFlags::from_bits_retain(cursor.read_u32());

        // Bytes still available inside this order after the 7-byte header
        // (controlFlags + orderSize + fieldFlags).
        let order_body_end = start.checked_sub(order_size)?; // remaining-len at order end
        let decoded = Self::decode_body(cursor, field_flags, order_body_end);

        // Advance to the exact end of this order regardless of how much of the
        // body we decoded, so the next order starts cleanly.
        while cursor.len() > order_body_end {
            cursor.read_u8();
        }

        decoded
    }

    fn decode_body(cursor: &mut ReadCursor<'_>, flags: WindowFieldFlags, order_body_end: usize) -> Option<WindowOrder> {
        if flags.contains(WindowFieldFlags::TYPE_DESKTOP) {
            return Some(Self::decode_desktop(cursor, flags, order_body_end));
        }

        // Both window and notify orders begin with a windowId.
        let window_id = read_u32_bounded(cursor, order_body_end)?;

        if flags.contains(WindowFieldFlags::TYPE_NOTIFY) {
            let notify_id = read_u32_bounded(cursor, order_body_end).unwrap_or(0);
            return Some(WindowOrder::NotifyIcon {
                window_id,
                notify_id,
                deleted: flags.contains(WindowFieldFlags::STATE_DELETED),
            });
        }

        // TYPE_WINDOW (the common case).
        if flags.contains(WindowFieldFlags::STATE_DELETED) {
            return Some(WindowOrder::DeleteWindow { window_id });
        }
        if flags.intersects(WindowFieldFlags::ICON | WindowFieldFlags::CACHED_ICON) {
            return Some(WindowOrder::WindowIcon {
                window_id,
                cached: flags.contains(WindowFieldFlags::CACHED_ICON),
            });
        }

        let state = Self::decode_window_state(cursor, flags, order_body_end);
        if flags.contains(WindowFieldFlags::STATE_NEW) {
            Some(WindowOrder::CreateWindow { window_id, state })
        } else {
            Some(WindowOrder::UpdateWindow { window_id, state })
        }
    }

    /// Decode the optional fields of a New-or-Existing Window order in spec order
    /// (§2.2.1.3.1.2.1). Any field that would exceed the order boundary is
    /// skipped, leaving its value `None`.
    fn decode_window_state(cursor: &mut ReadCursor<'_>, flags: WindowFieldFlags, end: usize) -> WindowState {
        let mut state = WindowState::default();

        if flags.contains(WindowFieldFlags::OWNER) {
            state.owner_window_id = read_u32_bounded(cursor, end);
        }
        if flags.contains(WindowFieldFlags::STYLE) {
            state.style = read_u32_bounded(cursor, end);
            state.extended_style = read_u32_bounded(cursor, end);
        }
        if flags.contains(WindowFieldFlags::SHOW) {
            state.show_state = read_u8_bounded(cursor, end);
        }
        if flags.contains(WindowFieldFlags::TITLE) {
            state.title = read_rail_unicode_string(cursor, end);
        }
        if flags.contains(WindowFieldFlags::CLIENT_AREA_OFFSET) {
            state.client_offset = read_point(cursor, end);
        }
        if flags.contains(WindowFieldFlags::CLIENT_AREA_SIZE) {
            state.client_area_size = read_size(cursor, end);
        }
        if flags.contains(WindowFieldFlags::RP_CONTENT) {
            read_u8_bounded(cursor, end);
        }
        if flags.contains(WindowFieldFlags::ROOT_PARENT) {
            read_u32_bounded(cursor, end);
        }
        if flags.contains(WindowFieldFlags::WND_OFFSET) {
            state.window_offset = read_point(cursor, end);
        }
        if flags.contains(WindowFieldFlags::WND_CLIENT_DELTA) {
            state.window_client_delta = read_point(cursor, end);
        }
        if flags.contains(WindowFieldFlags::WND_SIZE) {
            state.window_size = read_size(cursor, end);
        }
        // WND_RECTS / VIS_OFFSET / VISIBILITY follow but are not surfaced here;
        // the caller advances past them via orderSize.
        if flags.contains(WindowFieldFlags::VIS_OFFSET) {
            state.visible_offset = read_point(cursor, end);
        }

        state
    }

    fn decode_desktop(cursor: &mut ReadCursor<'_>, flags: WindowFieldFlags, end: usize) -> WindowOrder {
        // Non-monitored desktop carries no fields; monitored desktop carries an
        // ActiveWindowId and a z-ordered WindowIds array. We surface both.
        let active_window_id = read_u32_bounded(cursor, end).filter(|&id| id != 0);
        let num = read_u8_bounded(cursor, end).unwrap_or(0);
        let mut window_ids = Vec::new();
        for _ in 0..num {
            match read_u32_bounded(cursor, end) {
                Some(id) => window_ids.push(id),
                None => break,
            }
        }
        let _ = flags;
        WindowOrder::Desktop {
            active_window_id,
            window_ids,
        }
    }
}

fn read_u8_bounded(cursor: &mut ReadCursor<'_>, end: usize) -> Option<u8> {
    if cursor.len().checked_sub(1)? < end {
        return None;
    }
    Some(cursor.read_u8())
}

fn read_u32_bounded(cursor: &mut ReadCursor<'_>, end: usize) -> Option<u32> {
    if cursor.len().checked_sub(4)? < end {
        return None;
    }
    Some(cursor.read_u32())
}

fn read_point(cursor: &mut ReadCursor<'_>, end: usize) -> Option<Point> {
    let x = read_i32_bounded(cursor, end)?;
    let y = read_i32_bounded(cursor, end)?;
    Some(Point { x, y })
}

fn read_size(cursor: &mut ReadCursor<'_>, end: usize) -> Option<Size> {
    let width = read_i32_bounded(cursor, end)?;
    let height = read_i32_bounded(cursor, end)?;
    Some(Size { width, height })
}

fn read_i32_bounded(cursor: &mut ReadCursor<'_>, end: usize) -> Option<i32> {
    read_u32_bounded(cursor, end).map(|v| i32::from_le_bytes(v.to_le_bytes()))
}

/// `RAIL_UNICODE_STRING`: CbString (u16, byte length) + UTF-16LE string.
fn read_rail_unicode_string(cursor: &mut ReadCursor<'_>, end: usize) -> Option<String> {
    if cursor.len().checked_sub(2)? < end {
        return None;
    }
    let cb = usize::from(cursor.read_u16());
    if cb == 0 {
        return Some(String::new());
    }
    if cursor.len().checked_sub(cb)? < end {
        return None;
    }
    // `ensure_size!` is not used here because bounds are already checked above.
    let bytes = cursor.read_slice(cb);
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Some(String::from_utf16_lossy(&units))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Build one ALTSEC window order: controlFlags(WINDOW) + orderSize + fieldFlags + body.
    fn window_order(field_flags: u32, body: &[u8]) -> Vec<u8> {
        let control_flags = ALTSEC_WINDOW << 2; // TS_STANDARD/SECONDARY clear
        let order_size = u16::try_from(1 + 2 + 4 + body.len()).unwrap();
        let mut v = Vec::new();
        v.push(control_flags);
        v.extend_from_slice(&order_size.to_le_bytes());
        v.extend_from_slice(&field_flags.to_le_bytes());
        v.extend_from_slice(body);
        v
    }

    fn orders_update(orders: &[Vec<u8>]) -> Vec<u8> {
        let mut v = u16::try_from(orders.len()).unwrap().to_le_bytes().to_vec();
        for o in orders {
            v.extend_from_slice(o);
        }
        v
    }

    #[test]
    fn create_window_with_offset_size_and_title() {
        let flags = (WindowFieldFlags::TYPE_WINDOW
            | WindowFieldFlags::STATE_NEW
            | WindowFieldFlags::SHOW
            | WindowFieldFlags::TITLE
            | WindowFieldFlags::WND_OFFSET
            | WindowFieldFlags::WND_SIZE)
            .bits();

        let title: Vec<u8> = "App".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut body = Vec::new();
        body.extend_from_slice(&0x0000_00A2u32.to_le_bytes()); // windowId
        body.push(3); // showState (SW_MAXIMIZE)
        body.extend_from_slice(&u16::try_from(title.len()).unwrap().to_le_bytes()); // CbString
        body.extend_from_slice(&title);
        body.extend_from_slice(&120i32.to_le_bytes()); // windowOffsetX
        body.extend_from_slice(&80i32.to_le_bytes()); // windowOffsetY
        body.extend_from_slice(&1024i32.to_le_bytes()); // windowWidth
        body.extend_from_slice(&768i32.to_le_bytes()); // windowHeight

        let update = orders_update(&[window_order(flags, &body)]);
        let orders = WindowOrder::decode_orders_update(&update);

        assert_eq!(orders.len(), 1);
        let WindowOrder::CreateWindow { window_id, state } = &orders[0] else {
            panic!("expected CreateWindow, got {:?}", orders[0]);
        };
        assert_eq!(*window_id, 0xA2);
        assert_eq!(state.show_state, Some(3));
        assert_eq!(state.title.as_deref(), Some("App"));
        assert_eq!(state.window_offset, Some(Point { x: 120, y: 80 }));
        assert_eq!(
            state.window_size,
            Some(Size {
                width: 1024,
                height: 768
            })
        );
    }

    #[test]
    fn delete_window() {
        let flags = (WindowFieldFlags::TYPE_WINDOW | WindowFieldFlags::STATE_DELETED).bits();
        let body = 0x0000_00A2u32.to_le_bytes().to_vec();
        let update = orders_update(&[window_order(flags, &body)]);
        assert_eq!(
            WindowOrder::decode_orders_update(&update),
            vec![WindowOrder::DeleteWindow { window_id: 0xA2 }]
        );
    }

    #[test]
    fn two_orders_in_one_update() {
        let create = window_order(
            (WindowFieldFlags::TYPE_WINDOW | WindowFieldFlags::STATE_NEW).bits(),
            &1u32.to_le_bytes(),
        );
        let delete = window_order(
            (WindowFieldFlags::TYPE_WINDOW | WindowFieldFlags::STATE_DELETED).bits(),
            &1u32.to_le_bytes(),
        );
        let update = orders_update(&[create, delete]);
        let orders = WindowOrder::decode_orders_update(&update);
        assert_eq!(orders.len(), 2);
        assert!(matches!(orders[0], WindowOrder::CreateWindow { window_id: 1, .. }));
        assert_eq!(orders[1], WindowOrder::DeleteWindow { window_id: 1 });
    }

    #[test]
    fn stops_at_non_window_order() {
        // A create followed by a primary order (TS_STANDARD set) — the primary
        // must stop the walk, leaving the create decoded.
        let create = window_order(
            (WindowFieldFlags::TYPE_WINDOW | WindowFieldFlags::STATE_NEW).bits(),
            &7u32.to_le_bytes(),
        );
        let mut update = orders_update(&[create]);
        // bump numberOrders to 2 and append a primary-order controlFlags byte
        update[0..2].copy_from_slice(&2u16.to_le_bytes());
        update.push(CONTROL_FLAG_STANDARD);
        let orders = WindowOrder::decode_orders_update(&update);
        assert_eq!(orders.len(), 1);
        assert!(matches!(orders[0], WindowOrder::CreateWindow { window_id: 7, .. }));
    }
}
