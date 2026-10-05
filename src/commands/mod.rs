//! The lifecycle commands: capture, record, export, edit, doctor, update.
//!
//! `normalize` is no longer a user command — it runs automatically at the end of
//! `capture` — but its logic lives in [`crate::normalize`] and is invoked here.

pub mod capture;
pub mod control;
pub mod doctor;
pub mod edit;
pub mod edit_reveal;
pub mod export;
pub mod focus;
pub mod open;
pub mod record;
pub mod stop;
pub mod update;
