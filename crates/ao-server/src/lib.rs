// The matrix route handlers' futures embed `matrix_sdk::room::Room`
// futures (`leave_room_best_effort`), whose async layout exceeds the
// default query depth when monomorphized into axum handlers here.
#![recursion_limit = "256"]

pub mod channel_provisioning;
pub mod error;
pub mod log_buffer;
pub mod migrate_skills;
pub mod routes;
pub mod webhook_gateway;
pub mod workspace_lock;
