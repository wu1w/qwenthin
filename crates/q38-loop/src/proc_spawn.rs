//! Child-process flags for GUI/Electron hosts.
//!
//! On Windows a console-subsystem helper (git, bash, python, MCP) spawned from
//! a windowless sidecar can sit on a hidden console forever. `CREATE_NO_WINDOW`
//! is the same flag `media_exec` already uses for ffmpeg.

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn hide_window(cmd: &mut std::process::Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}

pub fn hide_window_async(cmd: &mut tokio::process::Command) {
    #[cfg(windows)]
    {
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let _ = cmd;
}
