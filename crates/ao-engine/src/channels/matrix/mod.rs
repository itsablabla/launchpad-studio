//! Matrix channel transport — see `guide/MATRIX_PLAN.md`.
//!
//! Slices so far: A2 landed the `matrix-sdk` adapter ([`client`]) and the
//! `ChannelTransport` sync loop ([`transport`]); A3 landed the inbound
//! pipeline ([`inbound`]); A4 lands the outbound relay + typing heartbeat
//! ([`outbound`]) and markdown→spec-HTML wire conversion ([`format`]).
//!
//! The whole module compiles away under `--no-default-features` (the
//! `matrix` cargo feature gates `dep:matrix-sdk`), so SDK types must never
//! be named outside this directory. The `pub use` below is the facade
//! ao-server's `routes/matrix.rs` consumes — plain strings and facade types
//! only, per D1.

pub(crate) mod client;
pub(crate) mod format;
pub(crate) mod inbound;
pub(crate) mod outbound;
pub(crate) mod transport;

// Facade for ao-server's setup routes (A4): token/password validation and
// the transport's two route-facing methods (`invalidate_binding`,
// `leave_room_best_effort`). No SDK types cross this boundary.
pub use client::{
    password_login, preflight_whoami, MatrixClientError, MatrixClientErrorKind, MatrixIdentity,
    PasswordLoginResult,
};
pub use transport::MatrixTransport;
