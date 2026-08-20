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
/// `TS_SECONDARY`. NOT used to classify altsec orders (see [`WindowOrder::decode_one`]); it
/// is part of the on-wire window-order control byte (`0x2E`) and is referenced by the tests
/// that build realistic orders.
#[allow(dead_code)]
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
        // Resize margins (§2.2.1.3.1.2.1). Each carries TWO u16s (left/right, top/bottom)
        // and sits between CLIENT_AREA_SIZE and RP_CONTENT. Not surfaced, but MUST be
        // consumed: skipping them desyncs every later field, not just one.
        const RESIZE_MARGIN_X = 0x0000_0080;
        const RESIZE_MARGIN_Y = 0x0800_0000;
        // Class / state bits.
        const TYPE_WINDOW = 0x0100_0000;
        const TYPE_NOTIFY = 0x0200_0000;
        const TYPE_DESKTOP = 0x0400_0000;
        // Desktop-order sub-fields (only meaningful with TYPE_DESKTOP).
        // NONE = the input desktop switched to one RAIL is NOT monitoring (Ctrl+Alt+Del /
        // lock / UAC secure desktop): no window fields follow and the client must stop
        // presenting RAIL windows and show the primary surface full-screen.
        const DESKTOP_NONE = 0x0000_0001;
        const DESKTOP_ZORDER = 0x0000_0010;
        const DESKTOP_ACTIVEWND = 0x0000_0020;
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
    /// A desktop-information order. `non_monitored` is set for a Non-Monitored Desktop order
    /// (`WINDOW_ORDER_FIELD_DESKTOP_NONE`): the input desktop switched to one RAIL isn't tracking
    /// (Ctrl+Alt+Del / lock / UAC secure desktop), so no window/z-order fields are present and the
    /// client must present the primary surface full-screen instead of clipping to RAIL windows.
    /// Otherwise it's an Actively Monitored Desktop order carrying an active window + z-order list.
    Desktop {
        non_monitored: bool,
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

        // An order is *alternate secondary* iff TS_STANDARD is clear — that ALONE selects the
        // altsec class; the TS_SECONDARY bit is NOT part of the test. This matches the RDP
        // server encoder and FreeRDP's own dispatch (`update_recv_order`, orders.c:
        // `if (!(controlFlags & ORDER_STANDARD)) -> altsec`). Real window orders on the wire
        // carry controlFlags = `ORDER_SECONDARY | (ORDER_TYPE_WINDOW << 2)` = 0x2E — i.e. the
        // SECONDARY bit IS set. A previous version also rejected SECONDARY, so every
        // server-encoded HiDef RAIL window order was silently dropped (no positions -> the
        // RemoteApp froze on the desktop frame). Only TS_STANDARD-set (primary/secondary)
        // orders stop the walk.
        if control_flags & CONTROL_FLAG_STANDARD != 0 {
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
        // resizeMarginLeft/Right and resizeMarginTop/Bottom (u16 each). Consumed, not surfaced.
        if flags.contains(WindowFieldFlags::RESIZE_MARGIN_X) {
            read_u16_bounded(cursor, end);
            read_u16_bounded(cursor, end);
        }
        if flags.contains(WindowFieldFlags::RESIZE_MARGIN_Y) {
            read_u16_bounded(cursor, end);
            read_u16_bounded(cursor, end);
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
        // WND_RECTS is VARIABLE length (numWindowRects u16 + n x RECT_16) and sits BETWEEN
        // WND_SIZE and VIS_OFFSET. It is not surfaced, but it must still be consumed: reading
        // VIS_OFFSET off the top of the rect array yields (count | left << 16, top | right << 16)
        // -- e.g. ~(6_553_601, 73_663_176) for one rect -- which clips any window to nothing.
        if flags.contains(WindowFieldFlags::WND_RECTS) {
            skip_rect16_array(cursor, end);
        }
        if flags.contains(WindowFieldFlags::VIS_OFFSET) {
            state.visible_offset = read_point(cursor, end);
        }
        // VISIBILITY (numVisibilityRects u16 + n x RECT_16) is the last field; nothing follows
        // it that we read, and the caller re-syncs on orderSize, so it needs no explicit skip.

        state
    }

    fn decode_desktop(cursor: &mut ReadCursor<'_>, flags: WindowFieldFlags, end: usize) -> WindowOrder {
        // Non-Monitored Desktop (DESKTOP_NONE): the secure-desktop / CAD signal. It carries NO
        // body fields, so surface the flag and stop — this is what tells the compositor to present
        // the primary surface full-screen instead of clipping to the (now-irrelevant) RAIL windows.
        if flags.contains(WindowFieldFlags::DESKTOP_NONE) {
            return WindowOrder::Desktop {
                non_monitored: true,
                active_window_id: None,
                window_ids: Vec::new(),
            };
        }
        // Actively Monitored Desktop: ActiveWindowId (if ACTIVEWND) then a z-ordered WindowIds
        // array (if ZORDER). Each is gated by its own field flag per MS-RDPERP §2.2.1.3.3.2.1.
        let active_window_id = if flags.contains(WindowFieldFlags::DESKTOP_ACTIVEWND) {
            read_u32_bounded(cursor, end).filter(|&id| id != 0)
        } else {
            None
        };
        let mut window_ids = Vec::new();
        if flags.contains(WindowFieldFlags::DESKTOP_ZORDER) {
            let num = read_u8_bounded(cursor, end).unwrap_or(0);
            for _ in 0..num {
                match read_u32_bounded(cursor, end) {
                    Some(id) => window_ids.push(id),
                    None => break,
                }
            }
        }
        WindowOrder::Desktop {
            non_monitored: false,
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

fn read_u16_bounded(cursor: &mut ReadCursor<'_>, end: usize) -> Option<u16> {
    if cursor.len().checked_sub(2)? < end {
        return None;
    }
    Some(cursor.read_u16())
}

/// Consume a `numRects` (u16) count followed by that many `RECT_16` (4 x u16 = 8 bytes).
/// Stops early rather than running past the order boundary.
fn skip_rect16_array(cursor: &mut ReadCursor<'_>, end: usize) -> Option<u16> {
    let count = read_u16_bounded(cursor, end)?;
    for _ in 0..count {
        if cursor.len().checked_sub(8)? < end {
            return None;
        }
        cursor.advance(8);
    }
    Some(count)
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
    // Matches the real RDP-server / FreeRDP encoding: `ORDER_SECONDARY | (ORDER_TYPE_WINDOW <<
    // 2)` = 0x2E — TS_STANDARD clear (=> altsec), TS_SECONDARY SET. Regression guard for the
    // bug where the decoder rejected the SECONDARY bit and dropped every window order.
    fn window_order(field_flags: u32, body: &[u8]) -> Vec<u8> {
        let control_flags = CONTROL_FLAG_SECONDARY | (ALTSEC_WINDOW << 2); // 0x2E, as on the wire
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

    /// Real-world File Explorer CREATE order captured off the wire by the proxy:
    /// fieldFlags = 0x1108df1e (TYPE_WINDOW | STATE_NEW | OWNER | STYLE | SHOW | TITLE |
    /// CLIENT_AREA_OFFSET | ROOT_PARENT | WND_OFFSET | WND_CLIENT_DELTA | WND_SIZE |
    /// WND_RECTS | VIS_OFFSET | VISIBILITY). This exercises the FULL field set — including
    /// the variable-length WND_RECTS / VISIBILITY arrays that sit before/around VIS_OFFSET —
    /// which the simpler test above does not. Confirms `window_offset` is extracted from a
    /// realistic HiDef RemoteApp window order (win 0x10120 -> (292,241), size 1030x487).
    #[test]
    fn create_window_realworld_full_field_set() {
        const FIELD_FLAGS: u32 = 0x1108_df1e;
        // Sanity: our flag bits decode to exactly the field set the proxy logged.
        let f = WindowFieldFlags::from_bits_retain(FIELD_FLAGS);
        assert!(f.contains(WindowFieldFlags::TYPE_WINDOW | WindowFieldFlags::STATE_NEW));
        assert!(f.contains(WindowFieldFlags::WND_OFFSET | WindowFieldFlags::WND_SIZE));
        assert!(f.contains(WindowFieldFlags::WND_RECTS | WindowFieldFlags::VISIBILITY | WindowFieldFlags::VIS_OFFSET));
        // 0x1108df1e sets neither CLIENT_AREA_SIZE (0x10000), RP_CONTENT (0x20000), ROOT_PARENT
        // (0x40000) nor RESIZE_MARGIN (0x80/0x08000000). Bit 19 (0x80000) is ENFORCE_SERVER_ZORDER,
        // which is a flag-only field carrying no body bytes (FreeRDP window.c never reads it).
        assert!(!f.contains(WindowFieldFlags::CLIENT_AREA_SIZE));

        // Body in MS-RDPERP 2.2.1.3.1.2.1 spec field order.
        let title: Vec<u8> = "Sales".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
        let mut body = Vec::new();
        body.extend_from_slice(&0x0001_0120u32.to_le_bytes()); // windowId
        body.extend_from_slice(&0u32.to_le_bytes()); // OWNER: ownerWindowId
        body.extend_from_slice(&0x1600_0000u32.to_le_bytes()); // STYLE: style
        body.extend_from_slice(&0x0000_0100u32.to_le_bytes()); // STYLE: extendedStyle
        body.push(5); // SHOW: showState
        body.extend_from_slice(&u16::try_from(title.len()).unwrap().to_le_bytes()); // TITLE: CbString
        body.extend_from_slice(&title); // TITLE: string
        body.extend_from_slice(&292i32.to_le_bytes()); // CLIENT_AREA_OFFSET: X
        body.extend_from_slice(&268i32.to_le_bytes()); // CLIENT_AREA_OFFSET: Y
        // (no ROOT_PARENT / RP_CONTENT / RESIZE_MARGIN — not set in 0x1108df1e)
        body.extend_from_slice(&292i32.to_le_bytes()); // WND_OFFSET: X  <-- target
        body.extend_from_slice(&241i32.to_le_bytes()); // WND_OFFSET: Y  <-- target
        body.extend_from_slice(&0i32.to_le_bytes()); // WND_CLIENT_DELTA: X
        body.extend_from_slice(&27i32.to_le_bytes()); // WND_CLIENT_DELTA: Y
        body.extend_from_slice(&1030i32.to_le_bytes()); // WND_SIZE: width
        body.extend_from_slice(&487i32.to_le_bytes()); // WND_SIZE: height
        body.extend_from_slice(&1u16.to_le_bytes()); // WND_RECTS: numRects
        for v in [292u16, 241, 1322, 728] {
            body.extend_from_slice(&v.to_le_bytes()); // one rect: L,T,R,B
        }
        body.extend_from_slice(&292i32.to_le_bytes()); // VIS_OFFSET: X
        body.extend_from_slice(&241i32.to_le_bytes()); // VIS_OFFSET: Y
        body.extend_from_slice(&1u16.to_le_bytes()); // VISIBILITY: numRects
        for v in [292u16, 241, 1322, 728] {
            body.extend_from_slice(&v.to_le_bytes());
        }

        let update = orders_update(&[window_order(FIELD_FLAGS, &body)]);
        let orders = WindowOrder::decode_orders_update(&update);

        assert_eq!(orders.len(), 1, "expected exactly one CreateWindow");
        let WindowOrder::CreateWindow { window_id, state } = &orders[0] else {
            panic!("expected CreateWindow, got {:?}", orders[0]);
        };
        assert_eq!(*window_id, 0x1_0120);
        assert_eq!(
            state.window_offset,
            Some(Point { x: 292, y: 241 }),
            "window_offset must decode from the real full field set"
        );
        assert_eq!(
            state.window_size,
            Some(Size {
                width: 1030,
                height: 487
            })
        );
        // The field that regressed: VIS_OFFSET sits AFTER the variable-length WND_RECTS array.
        // Reading it without consuming the rects yields (numRects | left << 16, top | right << 16)
        // = (0x0124_0001, 0x052A_00F1) = (19_136_513, 86_638_833) here.
        assert_eq!(
            state.visible_offset,
            Some(Point { x: 292, y: 241 }),
            "visible_offset must be read AFTER the WND_RECTS array, not off the top of it"
        );
    }

    /// Ground truth handed over by the proxy from a live AVD RemoteApp session (2026-08-19):
    /// the dominant CREATE fieldFlags on AVD is 0x1100df1e, WND_RECTS is present in essentially
    /// every order with numWindowRects = 1, and `visibleOffset == windowOffset` in every sample
    /// captured (no >=6-digit offset appeared anywhere in the scan). That equality is the
    /// assertion, and it is exactly what the pre-fix parser could not produce.
    #[test]
    fn avd_capture_visible_offset_equals_window_offset() {
        const FIELD_FLAGS: u32 = 0x1100_df1e;
        let f = WindowFieldFlags::from_bits_retain(FIELD_FLAGS);
        assert!(f.contains(WindowFieldFlags::WND_RECTS | WindowFieldFlags::VIS_OFFSET | WindowFieldFlags::VISIBILITY));
        // The proxy scanned 17 distinct fieldFlags values in the capture; none set either
        // resize-margin bit, so those stay correct-but-unexercised on AVD.
        assert!(!f.intersects(WindowFieldFlags::RESIZE_MARGIN_X | WindowFieldFlags::RESIZE_MARGIN_Y));

        for (window_id, ox, oy, w, h) in [
            (0x0002_0100u32, 0i32, 0i32, 1024i32, 768i32),
            (0x0002_0200, 1304, 915, 600, 400),
            (0x0002_0300, 1455, 293, 820, 610),
        ] {
            let title: Vec<u8> = "App".encode_utf16().flat_map(|u| u.to_le_bytes()).collect();
            let mut body = Vec::new();
            body.extend_from_slice(&window_id.to_le_bytes());
            body.extend_from_slice(&0u32.to_le_bytes()); // OWNER
            body.extend_from_slice(&0x1600_0000u32.to_le_bytes()); // STYLE
            body.extend_from_slice(&0u32.to_le_bytes()); // extendedStyle
            body.push(5); // SHOW
            body.extend_from_slice(&u16::try_from(title.len()).unwrap().to_le_bytes());
            body.extend_from_slice(&title);
            body.extend_from_slice(&ox.to_le_bytes()); // CLIENT_AREA_OFFSET X
            body.extend_from_slice(&oy.to_le_bytes()); // CLIENT_AREA_OFFSET Y
            body.extend_from_slice(&ox.to_le_bytes()); // WND_OFFSET X
            body.extend_from_slice(&oy.to_le_bytes()); // WND_OFFSET Y
            body.extend_from_slice(&0i32.to_le_bytes()); // WND_CLIENT_DELTA X
            body.extend_from_slice(&0i32.to_le_bytes()); // WND_CLIENT_DELTA Y
            body.extend_from_slice(&w.to_le_bytes()); // WND_SIZE
            body.extend_from_slice(&h.to_le_bytes());
            body.extend_from_slice(&1u16.to_le_bytes()); // WND_RECTS: numWindowRects = 1
            for v in [
                u16::try_from(ox).unwrap(),
                u16::try_from(oy).unwrap(),
                u16::try_from(ox + w).unwrap(),
                u16::try_from(oy + h).unwrap(),
            ] {
                body.extend_from_slice(&v.to_le_bytes());
            }
            body.extend_from_slice(&ox.to_le_bytes()); // VIS_OFFSET X
            body.extend_from_slice(&oy.to_le_bytes()); // VIS_OFFSET Y
            body.extend_from_slice(&0u16.to_le_bytes()); // VISIBILITY: numVisibilityRects = 0

            let update = orders_update(&[window_order(FIELD_FLAGS, &body)]);
            let orders = WindowOrder::decode_orders_update(&update);
            let WindowOrder::CreateWindow { state, .. } = &orders[0] else {
                panic!("expected CreateWindow, got {:?}", orders[0]);
            };
            assert_eq!(state.window_offset, Some(Point { x: ox, y: oy }));
            assert_eq!(
                state.visible_offset, state.window_offset,
                "AVD ground truth: visibleOffset == windowOffset for window {window_id:#x}"
            );
        }
    }

    /// No host in this deployment sets the resize-margin bits, so this is the only coverage they
    /// get. It is worth having: unlike the VIS_OFFSET bug (one corrupt field, no desync, because
    /// it is the last field we read), an unconsumed margin sits EARLY and shifts every later
    /// field -- offsets and sizes included.
    #[test]
    fn resize_margins_are_consumed_so_later_fields_stay_aligned() {
        const FIELD_FLAGS: u32 = 0x1000_0000 // STATE_NEW
            | 0x0100_0000 // TYPE_WINDOW
            | 0x0001_0000 // CLIENT_AREA_SIZE
            | 0x0000_0080 // RESIZE_MARGIN_X
            | 0x0800_0000 // RESIZE_MARGIN_Y
            | 0x0000_0800 // WND_OFFSET
            | 0x0000_0400 // WND_SIZE
            | 0x0000_0100 // WND_RECTS
            | 0x0000_1000; // VIS_OFFSET

        let mut body = Vec::new();
        body.extend_from_slice(&0x55u32.to_le_bytes()); // windowId
        body.extend_from_slice(&800i32.to_le_bytes()); // CLIENT_AREA_SIZE: width
        body.extend_from_slice(&600i32.to_le_bytes()); // CLIENT_AREA_SIZE: height
        body.extend_from_slice(&8u16.to_le_bytes()); // RESIZE_MARGIN_X: left
        body.extend_from_slice(&8u16.to_le_bytes()); // RESIZE_MARGIN_X: right
        body.extend_from_slice(&4u16.to_le_bytes()); // RESIZE_MARGIN_Y: top
        body.extend_from_slice(&4u16.to_le_bytes()); // RESIZE_MARGIN_Y: bottom
        body.extend_from_slice(&70i32.to_le_bytes()); // WND_OFFSET: X
        body.extend_from_slice(&90i32.to_le_bytes()); // WND_OFFSET: Y
        body.extend_from_slice(&816i32.to_le_bytes()); // WND_SIZE: width
        body.extend_from_slice(&608i32.to_le_bytes()); // WND_SIZE: height
        body.extend_from_slice(&2u16.to_le_bytes()); // WND_RECTS: TWO rects
        for v in [70u16, 90, 886, 394, 70, 394, 886, 698] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(&70i32.to_le_bytes()); // VIS_OFFSET: X
        body.extend_from_slice(&90i32.to_le_bytes()); // VIS_OFFSET: Y

        let update = orders_update(&[window_order(FIELD_FLAGS, &body)]);
        let orders = WindowOrder::decode_orders_update(&update);
        let WindowOrder::CreateWindow { state, .. } = &orders[0] else {
            panic!("expected CreateWindow, got {:?}", orders[0]);
        };
        assert_eq!(
            state.client_area_size,
            Some(Size {
                width: 800,
                height: 600
            })
        );
        assert_eq!(state.window_offset, Some(Point { x: 70, y: 90 }));
        assert_eq!(
            state.window_size,
            Some(Size {
                width: 816,
                height: 608
            })
        );
        assert_eq!(
            state.visible_offset,
            Some(Point { x: 70, y: 90 }),
            "a multi-rect WND_RECTS array must be skipped by count, not by a fixed size"
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
