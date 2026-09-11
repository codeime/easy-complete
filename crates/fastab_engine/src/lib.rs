//! Headless completion engine.
//!
//! Bundled Fig specs are compiled to static JSON IR at build time. Lookup and
//! generators run in Rust.

mod cobra;
mod filegen;
mod generate;
mod history;
mod ir;
mod js_host;
mod lookup;
mod process;
mod query;
mod rank;
mod runtime;
mod worker;

pub use ir::{ArgSpec, Builtin, OptionSpec, Registry, Spec, Template};
pub use lookup::{completion_buffer, current_command_slice, tokenize};
pub use rank::{ACCEPTANCE_STATE_KEY, AcceptanceIndex};
pub use runtime::{CompleteRequest, CompleteResult, CurrentArg, Engine, Suggestion, ranking_root_command};
pub use worker::{EngineClient, default_specs_dir, engine_attempt_timeout, ui_completion_deadline};
