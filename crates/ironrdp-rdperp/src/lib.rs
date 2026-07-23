//! RAIL — Remote Programs / Remote Applications Integrated Locally (MS-RDPERP).
//!
//! This crate implements the `rail` static virtual channel PDUs and the two
//! RAIL capability sets. It lets a client negotiate RemoteApp mode, launch a
//! published application via a Client Execute PDU (whose `arguments` field
//! carries the command line natively), and exchange system parameters.
//!
//! Server → client Window List orders (window create/update/delete, icons,
//! monitored desktop) are drawing orders that travel in the graphics update
//! stream rather than on this channel; they are handled elsewhere.
//!
//! Scope for now is the control channel (§2.2.2) and capabilities (§2.2.1.1).

pub mod capabilities;
pub mod client;
pub mod orders;
pub mod pdu;

/// The static virtual channel name for RAIL (MS-RDPERP §1.5). Null-terminated,
/// padded to the 8-byte channel-name field by the SVC layer.
pub const CHANNEL_NAME: &str = "rail";
