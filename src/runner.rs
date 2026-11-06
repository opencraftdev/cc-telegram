//! Spawn `claude -p`, parse its stream-json output, enforce timeout / stop, kill the process group.

use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Notify, mpsc};

pub struct RunOpts {
    pub claude_bin: String,
    pub prompt: String,
    pub system_prompt: String,
    pub model: String,
    pub max_turns: u32,
    pub resume: Option<String>,
    pub extra_args: Vec<String>,
    pub cwd: PathBuf,
    pub timeout: Duration,
}

#[derive(Clone, Debug, PartialEq)]
pub struct RunResult {
    pub text: String,
    pub session_id: Option<String>,
    pub cost_usd: f64,
    pub is_error: bool,
    pub errors: Vec<String>,
}

impl RunResult {
    /// `--resume` pointed at a session Claude Code no longer has.
    pub fn is_stale_session(&self) -> bool {
        self.is_error && is_stale_text(&format!("{} {}", self.text, self.errors.join(" ")))
    }
}

/// Error text that means the `--resume` session is missing or corrupt.
pub fn is_stale_text(text: &str) -> bool {
    let all = text.to_lowercase();
    let session_gone = all.contains("session")
        && ["not found", "invalid", "corrupt"]
            .iter()
            .any(|w| all.contains(w));
    all.contains("no conversation found") || session_gone
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Init(String),
    Tool(String),
    Result(RunResult),
}

#[derive(Debug)]
pub enum Outcome {
    Done(RunResult),
    /// Timed out / stopped; carries the session id seen in `system/init`, if any.
    TimedOut(Option<String>),
    Stopped(Option<String>),
    Failed(String),
}

pub fn build_args(o: &RunOpts) -> Vec<String> {
    let mut a: Vec<String> = [
        "-p",
        "--output-format",
        "stream-json",
        "--verbose",
        "--append-system-prompt",
        &o.system_prompt,
        "--dangerously-skip-permissions",
        "--model",
        &o.model,
        "--max-turns",
        &o.max_turns.to_string(),
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(id) = &o.resume {
        a.extend(["--resume".to_string(), id.clone()]);
    }
    a.extend(o.extra_args.iter().cloned());
    // `--` so a prompt starting with '-' is never parsed as a flag.
    a.extend(["--".to_string(), o.prompt.clone()]);
    a
}

/// Parse one stream-json line. Unknown events / fields are ignored.
pub fn parse_line(line: &str) -> Vec<Event> {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return vec![];
    };
    let s = |v: &Value| v.as_str().map(str::to_string);
    match v["type"].as_str() {
        Some("system") if v["subtype"] == "init" => {
            s(&v["session_id"]).map(Event::Init).into_iter().collect()
        }
        Some("assistant") => v["message"]["content"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| {
                let name = b["name"].as_str().unwrap_or("tool");
                Event::Tool(format!("🔧 {name}: {}", short_input(&b["input"])))
            })
            .collect(),
        Some("result") => vec![Event::Result(RunResult {
            text: s(&v["result"]).unwrap_or_default(),
            session_id: s(&v["session_id"]),
            cost_usd: v["total_cost_usd"].as_f64().unwrap_or(0.0),
            is_error: v["is_error"].as_bool().unwrap_or(false),
            errors: v["errors"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(s)
                .collect(),
        })],
        _ => vec![],
    }
}

fn short_input(input: &Value) -> String {
    let keys = [
        "command",
        "file_path",
        "path",
        "pattern",
        "url",
        "query",
        "description",
        "prompt",
    ];
    let raw = keys
        .iter()
        .find_map(|k| input[*k].as_str().map(str::to_string))
        .unwrap_or_else(|| match input {
            Value::Null => String::new(),
            v => v.to_string(),
        });
    let one_line = raw.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate(&one_line, 60)
}

pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

fn kill_group(pid: Option<u32>) {
    if let Some(pid) = pid {
        // process_group(0) made the child a group leader: -pid hits it and all its descendants.
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

/// Process groups of runs in flight, so a SIGTERM'd daemon can take them down with it.
static RUNNING: Mutex<Vec<u32>> = Mutex::new(Vec::new());

/// Kill every claude process group this process started (used on shutdown).
pub fn kill_all() {
    for pid in RUNNING.lock().unwrap().drain(..) {
        kill_group(Some(pid));
    }
}

/// How long claude gets to exit (and stderr to drain) after its `result` / EOF.
const GRACE: Duration = Duration::from_secs(5);

/// Run claude; tool steps go to `steps`. `stop` cancels the run.
pub async fn run(o: &RunOpts, steps: mpsc::UnboundedSender<String>, stop: Arc<Notify>) -> Outcome {
    let mut child = match Command::new(&o.claude_bin)
        .args(build_args(o))
        .current_dir(&o.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(format!("could not start `{}`: {e}", o.claude_bin)),
    };
    let pid = child.id();
    RUNNING.lock().unwrap().extend(pid);
    let out = drive(o, &mut child, steps, stop).await;
    RUNNING.lock().unwrap().retain(|p| Some(*p) != pid);
    out
}

async fn drive(
    o: &RunOpts,
    child: &mut tokio::process::Child,
    steps: mpsc::UnboundedSender<String>,
    stop: Arc<Notify>,
) -> Outcome {
    let pid = child.id();
    let mut stderr = child.stderr.take().expect("piped stderr");
    let mut stderr_task = tokio::spawn(async move {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s).await;
        s
    });
    let mut lines = BufReader::new(child.stdout.take().expect("piped stdout")).lines();
    let deadline = tokio::time::sleep(o.timeout);
    tokio::pin!(deadline);
    let (mut session, mut result) = (None, None);

    let interrupted = loop {
        tokio::select! {
            line = lines.next_line() => match line {
                Ok(Some(l)) => for ev in parse_line(&l) {
                    match ev {
                        Event::Init(id) => session = Some(id),
                        Event::Tool(t) => { let _ = steps.send(t); }
                        Event::Result(r) => result = Some(r),
                    }
                },
                _ => break None,
            },
            _ = &mut deadline => break Some(Outcome::TimedOut(session.clone())),
            _ = stop.notified() => break Some(Outcome::Stopped(session.clone())),
        }
        // Don't wait for EOF: a grandchild may hold stdout open long after the answer.
        if result.is_some() {
            break None;
        }
    };
    if let Some(out) = interrupted {
        kill_group(pid);
        let _ = child.wait().await;
        stderr_task.abort();
        return out;
    }
    let status = match tokio::time::timeout(GRACE, child.wait()).await {
        Ok(s) => s.ok(),
        Err(_) => {
            kill_group(pid); // claude itself hung after answering
            child.wait().await.ok()
        }
    };
    if let Some(r) = result {
        stderr_task.abort();
        return Outcome::Done(r);
    }
    // A grandchild may also hold stderr: bound the read.
    let err = match tokio::time::timeout(GRACE, &mut stderr_task).await {
        Ok(s) => s.unwrap_or_default(),
        Err(_) => {
            stderr_task.abort();
            String::new()
        }
    };
    let tail: String = err
        .trim()
        .lines()
        .rev()
        .take(5)
        .collect::<Vec<_>>()
        .join("\n");
    Outcome::Failed(if tail.is_empty() {
        format!("claude exited without a result ({status:?})")
    } else {
        tail
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_real_event_shapes() {
        assert!(
            parse_line(r#"{"type":"system","subtype":"hook_started","session_id":"x"}"#).is_empty()
        );
        assert_eq!(
            parse_line(
                r#"{"type":"system","subtype":"init","cwd":"/w","session_id":"s1","tools":["Bash"]}"#
            ),
            vec![Event::Init("s1".into())]
        );
        let tool = r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":""},{"type":"tool_use","id":"t","name":"Bash","input":{"command":"echo   hi\nthere","description":"Print"}},{"type":"tool_use","name":"Read","input":{"file_path":"/a/b.rs"}}]}}"#;
        assert_eq!(
            parse_line(tool),
            vec![
                Event::Tool("🔧 Bash: echo hi there".into()),
                Event::Tool("🔧 Read: /a/b.rs".into())
            ]
        );
        assert!(
            parse_line(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#
            )
            .is_empty()
        );
        let res = r#"{"type":"result","subtype":"success","is_error":false,"result":"hi","session_id":"s1","total_cost_usd":0.0075,"usage":{}}"#;
        assert_eq!(
            parse_line(res),
            vec![Event::Result(RunResult {
                text: "hi".into(),
                session_id: Some("s1".into()),
                cost_usd: 0.0075,
                is_error: false,
                errors: vec![],
            })]
        );
        assert!(parse_line("not json").is_empty());
        assert!(parse_line(r#"{"type":"rate_limit_event"}"#).is_empty());
    }

    #[test]
    fn detects_stale_resume() {
        let line = r#"{"type":"result","subtype":"error_during_execution","is_error":true,"session_id":"0000","total_cost_usd":0,"errors":["No conversation found with session ID: 0000"]}"#;
        let Event::Result(r) = &parse_line(line)[0] else {
            panic!()
        };
        assert!(r.is_stale_session());
        let ok = RunResult {
            is_error: false,
            ..r.clone()
        };
        assert!(!ok.is_stale_session());
        // stderr-only failure from a bogus --resume
        assert!(is_stale_text("No conversation found with session ID: 0000"));
        assert!(!is_stale_text("Error: rate limited"));
    }

    #[tokio::test]
    async fn answer_returned_even_if_grandchild_holds_pipes() {
        let o = fake_opts(
            r#"echo '{"type":"result","result":"fast","session_id":"s","total_cost_usd":0,"is_error":false}'
sleep 20 &"#,
            Duration::from_secs(15),
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        let t = std::time::Instant::now();
        let Outcome::Done(r) = run(&o, tx, Arc::new(Notify::new())).await else {
            panic!()
        };
        assert_eq!(r.text, "fast");
        assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
    }

    #[test]
    fn args_are_a_vector_with_prompt_last() {
        let o = RunOpts {
            claude_bin: "claude".into(),
            prompt: "--rm -rf; $(x)".into(),
            system_prompt: "sys".into(),
            model: "haiku".into(),
            max_turns: 3,
            resume: Some("sid".into()),
            extra_args: vec!["--add-dir".into(), "/x".into()],
            cwd: "/".into(),
            timeout: Duration::from_secs(1),
        };
        let a = build_args(&o);
        assert_eq!(a[0], "-p");
        assert_eq!(&a[a.len() - 2..], ["--", "--rm -rf; $(x)"]);
        let pos = |f: &str| a.iter().position(|x| x == f).unwrap();
        assert_eq!(a[pos("--resume") + 1], "sid");
        assert_eq!(a[pos("--model") + 1], "haiku");
        assert_eq!(a[pos("--max-turns") + 1], "3");
        assert_eq!(a[pos("--append-system-prompt") + 1], "sys");
        assert!(pos("--add-dir") < pos("--"));
    }

    fn fake_opts(script: &str, timeout: Duration) -> RunOpts {
        // A shell script stands in for the claude binary (it ignores the args).
        static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("cct-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let bin = dir.join(format!("fake-{n}.sh"));
        std::fs::write(&bin, format!("#!/bin/sh\n{script}\n")).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).unwrap();
        RunOpts {
            claude_bin: bin.to_string_lossy().into(),
            prompt: "p".into(),
            system_prompt: "s".into(),
            model: "m".into(),
            max_turns: 1,
            resume: None,
            extra_args: vec![],
            cwd: dir,
            timeout,
        }
    }

    #[tokio::test]
    async fn runs_fake_claude_and_kills_group_on_timeout() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ok = fake_opts(
            r#"echo '{"type":"system","subtype":"init","session_id":"s9"}'
echo '{"type":"assistant","message":{"content":[{"type":"tool_use","name":"Bash","input":{"command":"ls"}}]}}'
echo '{"type":"result","result":"done","session_id":"s9","total_cost_usd":0.01,"is_error":false}'"#,
            Duration::from_secs(10),
        );
        let Outcome::Done(r) = run(&ok, tx, Arc::new(Notify::new())).await else {
            panic!()
        };
        assert_eq!(r.text, "done");
        assert_eq!(rx.recv().await.unwrap(), "🔧 Bash: ls");

        // A grandchild that would outlive a plain child kill.
        let marker = std::env::temp_dir().join(format!("cct-marker-{}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let slow = fake_opts(
            &format!(
                "echo '{{\"type\":\"system\",\"subtype\":\"init\",\"session_id\":\"s7\"}}'\n(sleep 3; touch {}) &\nsleep 30",
                marker.display()
            ),
            Duration::from_millis(1500),
        );
        let (tx, _rx) = mpsc::unbounded_channel();
        match run(&slow, tx, Arc::new(Notify::new())).await {
            Outcome::TimedOut(s) => assert_eq!(s.as_deref(), Some("s7")),
            o => panic!("{o:?}"),
        }
        tokio::time::sleep(Duration::from_secs(4)).await;
        assert!(!marker.exists(), "grandchild survived the group kill");
    }

    #[tokio::test]
    async fn stop_cancels_and_missing_binary_fails() {
        let stop = Arc::new(Notify::new());
        stop.notify_one(); // permit stored before the run starts waiting
        let (tx, _rx) = mpsc::unbounded_channel();
        let o = fake_opts("sleep 30", Duration::from_secs(60));
        assert!(matches!(run(&o, tx, stop).await, Outcome::Stopped(None)));
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut o = fake_opts("true", Duration::from_secs(5));
        o.claude_bin = "/nonexistent/claude".into();
        assert!(matches!(
            run(&o, tx, Arc::new(Notify::new())).await,
            Outcome::Failed(_)
        ));
    }
}
