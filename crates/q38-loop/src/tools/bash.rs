//! `bash`. QwenPaw `execute_shell_command`: fresh subprocess, workspace cwd, formatted output.

use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use super::{arg_str, folded_response, BlobStore, ToolLimits, Workspace};
use crate::tool_calls::{CancelFlag, ToolCall, ToolResponse, ToolState};

const OUTPUT_MAX_BYTES: usize = 1024 * 1024;
/// shell 退出后管道的收尾读窗口：孙进程（`sleep 30 & echo hi`）继承了
/// stdout/stderr 写端，EOF 可能永远不来，超时就放弃。
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);

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

fn spawn_shell(command: &str, cwd: &std::path::Path) -> std::io::Result<tokio::process::Child> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.current_dir(cwd)
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

/// 取消路径的整组击杀。`setpgid(0,0)` 保证 pgid 就是子 shell 的 pid。
fn kill_group(child: &tokio::process::Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = child;
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
}
