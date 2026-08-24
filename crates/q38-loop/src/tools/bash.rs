//! `bash`. QwenPaw `execute_shell_command`: fresh subprocess, workspace cwd, formatted output.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{arg_str, folded_response, BlobStore, ToolLimits, Workspace};
use crate::tool_calls::{CancelFlag, ToolCall, ToolResponse, ToolState};

const OUTPUT_MAX_BYTES: usize = 1024 * 1024;
/// shell 退出后管道的收尾读窗口：孙进程（`sleep 30 & echo hi`）继承了
/// stdout/stderr 写端，EOF 可能永远不来，超时就放弃。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Debug)]
enum ShellKind {
    Bash,
    Posix,
    PowerShell,
}

#[derive(Clone, Debug)]
struct ShellSpec {
    exe: PathBuf,
    kind: ShellKind,
}

static SHELL: OnceLock<ShellSpec> = OnceLock::new();

#[cfg(windows)]
struct WindowsJob(windows_sys::Win32::Foundation::HANDLE);

#[cfg(windows)]
impl WindowsJob {
    fn attach(pid: u32) -> std::io::Result<Self> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use windows_sys::Win32::System::Threading::{
            OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE,
        };

        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() || job == INVALID_HANDLE_VALUE {
                return Err(std::io::Error::last_os_error());
            }
            let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
            info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                let e = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(e);
            }
            let process = OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid);
            if process.is_null() || process == INVALID_HANDLE_VALUE {
                let e = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(e);
            }
            let assigned = AssignProcessToJobObject(job, process);
            CloseHandle(process);
            if assigned == 0 {
                let e = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(e);
            }
            Ok(Self(job))
        }
    }
}

#[cfg(windows)]
impl Drop for WindowsJob {
    fn drop(&mut self) {
        unsafe {
            windows_sys::Win32::Foundation::CloseHandle(self.0);
        }
    }
}

#[cfg(windows)]
unsafe impl Send for WindowsJob {}

pub async fn bash(
    ws: &Workspace,
    call: &ToolCall,
    cancel: CancelFlag,
    limits: ToolLimits,
    blobs: Option<&BlobStore>,
) -> ToolResponse {
    let Some(command) = arg_str(&call.arguments, "command") else {
        return ToolResponse::text(&call.id, "Error: No `command` provided.", ToolState::Error);
    };
    let command = command.trim().to_string();
    if command.is_empty() {
        return ToolResponse::text(&call.id, "Error: No `command` provided.", ToolState::Error);
    }

    let mut child = match spawn_shell(&command, ws.root()) {
        Ok(c) => c,
        Err(e) => {
            return ToolResponse::text(
                &call.id,
                format!("Error: failed to spawn shell: {e}"),
                ToolState::Error,
            );
        }
    };
    // A per-call Windows Job gives cancellation/drop the same descendant-tree
    // semantics as the Unix process group. Failure is non-fatal on restricted
    // hosts; direct-child cancellation still works.
    #[cfg(windows)]
    let _job = child.id().and_then(|pid| WindowsJob::attach(pid).ok());

    // 缓冲共享给读取任务：收尾读超时被 abort 时，已读到的部分不丢。
    let out_buf: Arc<Mutex<Vec<u8>>> = Arc::default();
    let err_buf: Arc<Mutex<Vec<u8>>> = Arc::default();
    let out_task = child
        .stdout
        .take()
        .map(|p| tokio::spawn(read_capped_into(p, out_buf.clone())));
    let err_task = child
        .stderr
        .take()
        .map(|p| tokio::spawn(read_capped_into(p, err_buf.clone())));

    tokio::select! {
        biased;
        _ = cancel.cancelled() => {
            // 杀整个进程组：只杀直接 shell 会留下孙进程（后台任务）继续
            // 持有管道写端，读取任务永远等不到 EOF。
            kill_group(&child);
            let _ = child.start_kill();
            let _ = child.wait().await;
            drain(out_task).await;
            drain(err_task).await;
            ToolResponse::text(
                &call.id,
                "Command failed with exit code -1.\n[stderr]\ncancelled",
                ToolState::Interrupted,
            )
        }
        status = child.wait() => {
            drain(out_task).await;
            drain(err_task).await;
            let stdout = take_text(&out_buf);
            let stderr = take_text(&err_buf);
            let code = status.ok().and_then(|s| s.code()).unwrap_or(-1);
            let text = format_shell(code, &stdout, &stderr);
            let state = if code == 0 {
                ToolState::Success
            } else {
                ToolState::Error
            };
            folded_response(&call.id, text, state, limits, blobs)
        }
    }
}

fn spawn_shell(command: &str, cwd: &Path) -> std::io::Result<tokio::process::Child> {
    let shell = SHELL.get_or_init(detect_shell);
    let mut cmd = Command::new(&shell.exe);
    match shell.kind {
        ShellKind::Bash => {
            // Keep one command dialect on macOS, Linux, and Windows/Git Bash.
            // Skipping profiles makes every tool call deterministic and cheap.
            cmd.args(["--noprofile", "--norc", "-c", command]);
        }
        ShellKind::Posix => {
            cmd.args(["-c", command]);
        }
        ShellKind::PowerShell => {
            cmd.args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                command,
            ]);
        }
    }
    cmd.current_dir(cwd)
        .env("PATH", tool_path())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // 独立进程组：pgid == 子 shell pid，取消时 kill(-pgid) 连孙进程一起杀。
    #[cfg(unix)]
    unsafe {
        cmd.pre_exec(|| {
            libc::setpgid(0, 0);
            Ok(())
        });
    }
    cmd.spawn()
}

/// Bash runs `--noprofile --norc`, so rustup's shell hook never runs.
/// GUI/Electron PATH is often just `/usr/bin:/bin`, which hides `~/.cargo/bin`.
fn tool_path() -> OsString {
    merge_tool_path(
        std::env::var_os("PATH"),
        crate::config::user_home().as_deref(),
        &extra_path_dirs(),
    )
}

fn extra_path_dirs() -> Vec<PathBuf> {
    static DIRS: OnceLock<Vec<PathBuf>> = OnceLock::new();
    DIRS.get_or_init(|| {
        let mut dirs = Vec::new();
        if let Ok(raw) = std::env::var("Q38_PATH") {
            for part in split_path_list(&raw) {
                if let Some(p) = expand_dir(&part) {
                    dirs.push(p);
                }
            }
        }
        for raw in crate::config::Config::load_file_or_default().tools.extra_path {
            if let Some(p) = expand_dir(&raw) {
                dirs.push(p);
            }
        }
        dirs
    })
    .clone()
}

fn split_path_list(raw: &str) -> Vec<String> {
    let sep = if cfg!(windows) { ';' } else { ':' };
    raw.split(sep)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn expand_dir(raw: &str) -> Option<PathBuf> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    if raw == "~" {
        return crate::config::user_home();
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        return Some(crate::config::user_home()?.join(rest));
    }
    #[cfg(windows)]
    if let Some(rest) = raw.strip_prefix("~\\") {
        return Some(crate::config::user_home()?.join(rest));
    }
    Some(PathBuf::from(raw))
}

fn well_known_bins(home: Option<&Path>) -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Some(home) = home {
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
        dirs.push(home.join("go/bin"));
        #[cfg(windows)]
        {
            dirs.push(home.join("scoop/shims"));
            dirs.push(home.join("AppData/Roaming/npm"));
            dirs.push(home.join("AppData/Local/Microsoft/WinGet/Links"));
        }
    }
    #[cfg(unix)]
    {
        dirs.push(PathBuf::from("/opt/homebrew/bin"));
        dirs.push(PathBuf::from("/opt/homebrew/sbin"));
        dirs.push(PathBuf::from("/usr/local/bin"));
        dirs.push(PathBuf::from("/usr/local/sbin"));
    }
    #[cfg(windows)]
    {
        for key in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(base) = std::env::var_os(key) {
                let base = PathBuf::from(base);
                dirs.push(base.join("Git/cmd"));
                dirs.push(base.join("Git/bin"));
                dirs.push(base.join("nodejs"));
            }
        }
        if let Some(data) = std::env::var_os("ProgramData") {
            dirs.push(PathBuf::from(data).join("chocolatey/bin"));
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let local = PathBuf::from(local);
            dirs.push(local.join("Microsoft/WinGet/Links"));
            dirs.push(local.join("Programs/Git/cmd"));
        }
    }
    dirs
}

fn merge_tool_path(
    current: Option<OsString>,
    home: Option<&Path>,
    extra: &[PathBuf],
) -> OsString {
    let mut ordered = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let push = |dir: PathBuf, ordered: &mut Vec<PathBuf>, seen: &mut std::collections::HashSet<PathBuf>| {
        if dir.as_os_str().is_empty() || !dir.is_dir() {
            return;
        }
        if seen.insert(dir.clone()) {
            ordered.push(dir);
        }
    };
    for dir in extra {
        push(dir.clone(), &mut ordered, &mut seen);
    }
    for dir in well_known_bins(home) {
        push(dir, &mut ordered, &mut seen);
    }
    if let Some(ref current) = current {
        for dir in std::env::split_paths(current) {
            push(dir, &mut ordered, &mut seen);
        }
    }
    std::env::join_paths(&ordered).unwrap_or_else(|_| current.unwrap_or_default())
}

fn detect_shell() -> ShellSpec {
    if let Some(exe) = std::env::var_os("Q38_SHELL").filter(|s| !s.is_empty()) {
        let exe = PathBuf::from(exe);
        let name = exe
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        return ShellSpec {
            exe,
            kind: if name.contains("powershell") || name.starts_with("pwsh") {
                ShellKind::PowerShell
            } else if name.starts_with("bash") {
                ShellKind::Bash
            } else {
                ShellKind::Posix
            },
        };
    }

    #[cfg(windows)]
    {
        // Git for Windows is already present on most developer machines and
        // gives the model the same learned command language as macOS/Linux.
        let mut candidates = Vec::new();
        for key in ["ProgramFiles", "ProgramFiles(x86)", "LOCALAPPDATA"] {
            if let Some(base) = std::env::var_os(key) {
                let base = PathBuf::from(base);
                candidates.push(base.join("Git/bin/bash.exe"));
                candidates.push(base.join("Programs/Git/bin/bash.exe"));
            }
        }
        if let Some(path_bash) = find_in_path("bash.exe", true) {
            candidates.push(path_bash);
        }
        if let Some(exe) = candidates.into_iter().find(|p| p.is_file()) {
            return ShellSpec {
                exe,
                kind: ShellKind::Bash,
            };
        }
        for name in ["pwsh.exe", "powershell.exe"] {
            if let Some(exe) = find_in_path(name, false) {
                return ShellSpec {
                    exe,
                    kind: ShellKind::PowerShell,
                };
            }
        }
        ShellSpec {
            exe: PathBuf::from("powershell.exe"),
            kind: ShellKind::PowerShell,
        }
    }

    #[cfg(not(windows))]
    {
        for exe in [PathBuf::from("/bin/bash"), PathBuf::from("/usr/bin/bash")] {
            if exe.is_file() {
                return ShellSpec {
                    exe,
                    kind: ShellKind::Bash,
                };
            }
        }
        if let Some(exe) = [PathBuf::from("/bin/sh"), PathBuf::from("/usr/bin/sh")]
            .into_iter()
            .find(|p| p.is_file())
        {
            ShellSpec {
                exe,
                kind: ShellKind::Posix,
            }
        } else {
            ShellSpec {
                exe: PathBuf::from("bash"),
                kind: ShellKind::Bash,
            }
        }
    }
}

#[cfg(windows)]
fn find_in_path(name: &str, git_only: bool) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|p| std::env::split_paths(&p).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|p| {
            p.is_file() && (!git_only || p.to_string_lossy().to_ascii_lowercase().contains("git"))
        })
}

/// 取消路径的整组击杀。`setpgid(0,0)` 保证 pgid 就是子 shell 的 pid。
fn kill_group(child: &tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(windows)]
    if let Some(pid) = child.id() {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// 限时收尾读：正常等 EOF，超时（孙进程仍握着写端）就 abort 放弃，
/// 共享缓冲里已读到的部分照常返回。
async fn drain(task: Option<tokio::task::JoinHandle<()>>) {
    let Some(mut task) = task else {
        return;
    };
    if tokio::time::timeout(DRAIN_TIMEOUT, &mut task)
        .await
        .is_err()
    {
        task.abort();
        // Await cancellation so the pipe handle is actually closed before
        // returning; otherwise a Windows grandchild can keep the test/process
        // alive until its own timeout even though the tool call already ended.
        let _ = task.await;
    }
}

fn take_text(buf: &Arc<Mutex<Vec<u8>>>) -> String {
    let b = buf.lock().unwrap_or_else(|e| e.into_inner());
    String::from_utf8_lossy(&b).into_owned()
}

fn format_shell(code: i32, stdout: &str, stderr: &str) -> String {
    if code == 0 {
        let mut text = if stdout.is_empty() {
            "Command executed successfully (no output).".to_string()
        } else {
            stdout.to_string()
        };
        if !stderr.is_empty() {
            text.push_str("\n[stderr]\n");
            text.push_str(stderr);
        }
        text
    } else {
        let mut parts = vec![format!("Command failed with exit code {code}.")];
        if !stdout.is_empty() {
            parts.push(format!("\n[stdout]\n{stdout}"));
        }
        if !stderr.is_empty() {
            parts.push(format!("\n[stderr]\n{stderr}"));
        }
        parts.concat()
    }
}

async fn read_capped_into<R: AsyncRead + Unpin>(mut pipe: R, buf: Arc<Mutex<Vec<u8>>>) {
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => {
                let mut b = buf.lock().unwrap_or_else(|e| e.into_inner());
                let room = OUTPUT_MAX_BYTES.saturating_sub(b.len());
                if room > 0 {
                    b.extend_from_slice(&chunk[..n.min(room)]);
                }
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tool_calls::ToolCall;
    use serde_json::json;
    use std::path::PathBuf;
    use tokio::io::AsyncWriteExt;

    fn scratch() -> (Workspace, PathBuf) {
        let dir = std::env::temp_dir().join(format!("q38-bash-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let w = Workspace::open(&dir, true).unwrap();
        (w, dir)
    }

    fn call(args: serde_json::Value) -> ToolCall {
        ToolCall {
            id: "t1".into(),
            name: "bash".into(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn read_capped_drains_past_cap_to_eof() {
        let (mut writer, reader) = tokio::io::duplex(8192);
        let buf: Arc<Mutex<Vec<u8>>> = Arc::default();
        let reader_task = tokio::spawn(read_capped_into(reader, buf.clone()));
        let writer_task = tokio::spawn(async move {
            let first = vec![b'a'; OUTPUT_MAX_BYTES + 256 * 1024];
            writer.write_all(&first).await?;
            writer.write_all(b"TAIL").await?;
            Ok::<_, std::io::Error>(())
        });
        tokio::time::timeout(std::time::Duration::from_secs(5), reader_task)
            .await
            .expect("read_capped hung")
            .expect("join");
        writer_task
            .await
            .expect("writer join")
            .expect("follow-on write must complete (pipe drained to EOF)");
        let out = take_text(&buf);
        assert_eq!(out.len(), OUTPUT_MAX_BYTES);
        assert!(out.starts_with("aaaa"));
        assert!(!out.contains("TAIL"));
    }

    #[tokio::test]
    async fn background_grandchild_does_not_hang_bash() {
        // 孙进程继承 stdout 写端：shell 退出后收尾读限时放弃，
        // 不能等 sleep 30 结束才返回。
        let (ws, dir) = scratch();
        let started = std::time::Instant::now();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            bash(
                &ws,
                &call(json!({"command": "sleep 30 & echo hi"})),
                CancelFlag::new(),
                ToolLimits::default(),
                None,
            ),
        )
        .await
        .expect("bash hung on background grandchild");
        assert!(
            started.elapsed() < std::time::Duration::from_secs(8),
            "took {:?}",
            started.elapsed()
        );
        assert_eq!(out.state, ToolState::Success, "{}", out.joined_text());
        assert!(out.joined_text().contains("hi"), "{}", out.joined_text());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn cancel_kills_grandchild_via_process_group() {
        let (ws, dir) = scratch();
        let cancel = CancelFlag::new();
        let pid_file = dir.join("pid.txt");
        // shell 把孙进程 pid 写盘后 wait 挂住，等 cancel 杀整组。
        let cmd = "sleep 30 & echo $! > pid.txt; wait".to_string();
        let task = {
            let ws = ws.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                bash(
                    &ws,
                    &call(json!({"command": cmd})),
                    cancel,
                    ToolLimits::default(),
                    None,
                )
                .await
            })
        };
        // 等孙进程 pid 落盘再取消。
        let mut pid: Option<i32> = None;
        for _ in 0..100 {
            if let Ok(s) = std::fs::read_to_string(&pid_file) {
                if let Ok(p) = s.trim().parse::<i32>() {
                    pid = Some(p);
                    break;
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let pid = pid.expect("grandchild pid never appeared");
        cancel.cancel();
        let out = tokio::time::timeout(std::time::Duration::from_secs(10), task)
            .await
            .expect("cancelled bash hung")
            .expect("join");
        assert_eq!(out.state, ToolState::Interrupted, "{}", out.joined_text());
        // kill(pid, 0) 返回 -1/ESRCH 即孙进程已死；收养/收尸留点余量。
        let mut dead = false;
        for _ in 0..100 {
            if unsafe { libc::kill(pid, 0) } == -1 {
                dead = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(dead, "grandchild {pid} survived the group kill");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bash_nonzero_exit_is_error() {
        let (ws, dir) = scratch();
        let out = bash(
            &ws,
            &call(json!({"command": "echo hello >&2; echo out; exit 1"})),
            CancelFlag::new(),
            ToolLimits::default(),
            None,
        )
        .await;
        assert_eq!(out.state, ToolState::Error, "{}", out.joined_text());
        let text = out.joined_text();
        assert!(text.contains("exit code 1"), "{text}");
        assert!(text.contains("out"), "{text}");
        assert!(text.contains("hello"), "{text}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn bash_large_stdout_returns_prefix_without_hang() {
        let (ws, dir) = scratch();
        std::fs::write(dir.join("big.txt"), "a".repeat(2_000_000)).unwrap();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            bash(
                &ws,
                &call(json!({"command": "cat big.txt"})),
                CancelFlag::new(),
                ToolLimits::default(),
                None,
            ),
        )
        .await
        .expect("bash hung on large stdout");
        assert_eq!(out.state, ToolState::Success, "{}", out.joined_text());
        let live = out.joined_text();
        assert!(live.contains("aaa"), "{live}");
        assert!(!live.contains("Command failed"), "{live}");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tool_path_prepends_existing_cargo_bin() {
        let home = std::env::temp_dir().join(format!("q38-path-{}", uuid::Uuid::new_v4().simple()));
        let cargo_bin = home.join(".cargo/bin");
        std::fs::create_dir_all(&cargo_bin).unwrap();
        let merged = merge_tool_path(
            Some("/usr/bin".into()),
            Some(home.as_path()),
            &[],
        );
        let dirs: Vec<_> = std::env::split_paths(&merged).collect();
        assert_eq!(dirs.first().map(PathBuf::as_path), Some(cargo_bin.as_path()));
        assert!(dirs.iter().any(|d| d == Path::new("/usr/bin")));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn tool_path_skips_missing_well_known_dirs() {
        let home = std::env::temp_dir().join(format!("q38-path-missing-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&home).unwrap();
        let merged = merge_tool_path(Some("/usr/bin".into()), Some(home.as_path()), &[]);
        let dirs: Vec<_> = std::env::split_paths(&merged).collect();
        assert!(!dirs.iter().any(|d| d.ends_with(".cargo/bin")));
        assert!(dirs.iter().any(|d| d == Path::new("/usr/bin")));
        let _ = std::fs::remove_dir_all(home);
    }

    #[test]
    fn tool_path_extra_dirs_win_over_well_known() {
        let root = std::env::temp_dir().join(format!("q38-path-extra-{}", uuid::Uuid::new_v4().simple()));
        let extra = root.join("extra");
        let cargo_bin = root.join("home/.cargo/bin");
        std::fs::create_dir_all(&extra).unwrap();
        std::fs::create_dir_all(&cargo_bin).unwrap();
        let merged = merge_tool_path(
            Some("/usr/bin".into()),
            Some(&root.join("home")),
            &[extra.clone()],
        );
        let dirs: Vec<_> = std::env::split_paths(&merged).collect();
        assert_eq!(dirs.first().map(PathBuf::as_path), Some(extra.as_path()));
        assert_eq!(dirs.get(1).map(PathBuf::as_path), Some(cargo_bin.as_path()));
        let _ = std::fs::remove_dir_all(root);
    }
}
