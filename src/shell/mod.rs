//! The application shell: the TUI wire, the face adapters that connect a
//! phi-* pack to the shell, and the shell-side services (commands,
//! context rotation, skills, the click mailbox, the canvas host).

pub mod browser_face;
pub mod canvas_host;
pub mod click_mailbox;
pub mod commands;
pub mod context_rotation;
pub mod file_face;
pub mod model_command;
pub mod skills;
pub mod wire;
