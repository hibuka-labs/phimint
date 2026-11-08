//! Model selection and routing: configuration, the model catalog, the
//! router that picks one, and the title generator that runs on it.

pub mod config;
pub mod identity;
pub mod persist;
pub mod picker_rows;
pub mod router;
mod scan;
pub mod store;
pub mod switch;
pub mod title;
