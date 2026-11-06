//! Spawn `claude -p`, parse its stream-json output, enforce timeout / stop, kill the process group.

use serde_json::Value;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Notify, Semaphore, SemaphorePermit, mpsc};

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
    /// Prompt-side tokens (input + cache read/creation) and output tokens, 0 if not reported.
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub duration_ms: u64,
}

impl RunResult {
    /// `--resume` pointed at a session Claude Code no longer has.
    pub fn is_stale_session(&self) -> bool {
        self.is_error && is_stale_text(&format!("{} {}", self.text, self.errors.join(" ")))
    }
}

/// The error claude prints for a `--resume` id it doesn't have (verified against claude 2.1.x:
/// "No conversation found with session ID: <id>").
pub fn is_stale_text(text: &str) -> bool {
    text.to_lowercase()
        .contains("no conversation found with session id")
}

#[derive(Debug, PartialEq)]
pub enum Event {
    Init(String),
    Tool { name: String, input: String },
    Text(String),
    Result(RunResult),
}

impl Event {
    /// Status-line form of a tool step (Telegram).
    pub fn tool_line(name: &str, input: &str) -> String {
        format!("🔧 {name}: {input}")
    }
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
    // A NUL byte anywhere (a file, a user message) would make spawn fail outright.
    a.into_iter().map(|x| x.replace('\0', "")).collect()
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
            .filter_map(|b| match b["type"].as_str() {
                Some("tool_use") => Some(Event::Tool {
                    name: b["name"].as_str().unwrap_or("tool").to_string(),
                    input: short_input(&b["input"]),
                }),
                Some("text") => s(&b["text"]).filter(|t| !t.is_empty()).map(Event::Text),
                _ => None,
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
            input_tokens: [
                "input_tokens",
                "cache_read_input_tokens",
                "cache_creation_input_tokens",
            ]
            .iter()
            .filter_map(|k| v["usage"][*k].as_u64())
            .sum(),
            output_tokens: v["usage"]["output_tokens"].as_u64().unwrap_or(0),
            duration_ms: v["duration_ms"].as_u64().unwrap_or(0),
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

pub fn kill_group(pid: Option<u32>) {
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

/// Process-wide cap on concurrent claude processes (each is ~200-400MB of Node), shared by
/// every gateway and profile in this process. Waiters queue FIFO.
pub struct RunSlots {
    sem: Semaphore,
    waiting: AtomicUsize,
}

impl RunSlots {
    pub fn new(n: usize) -> Self {
        Self {
            sem: Semaphore::new(n.max(1)),
            waiting: AtomicUsize::new(0),
        }
    }

    /// Wait for a slot; None if `stop` fired first.
    pub async fn acquire(&self, stop: &Notify) -> Option<SemaphorePermit<'_>> {
        self.waiting.fetch_add(1, Ordering::SeqCst);
        let p = tokio::select! {
            p = self.sem.acquire() => p.ok(),
            _ = stop.notified() => None,
        };
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        p
    }

    /// `Some(runs already waiting)` when no slot is free right now.
    pub fn queued_ahead(&self) -> Option<usize> {
        (self.sem.available_permits() == 0).then(|| self.waiting.load(Ordering::SeqCst))
    }
}

pub const DEFAULT_MAX_RUNS: usize = 2;
static SLOTS: OnceLock<RunSlots> = OnceLock::new();

/// Set the process-wide cap (first call wins; call before any run starts).
pub fn set_max_runs(n: usize) {
    let _ = SLOTS.set(RunSlots::new(n));
}

pub fn slots() -> &'static RunSlots {
    // Tests run many fake claude processes in parallel; don't serialize them.
    SLOTS.get_or_init(|| RunSlots::new(if cfg!(test) { 64 } else { DEFAULT_MAX_RUNS }))
}

/// Run claude; Init/Tool/Text events go to `steps` as they stream. `stop` cancels the run,
/// also while it is still waiting for a global run slot.
pub async fn run(o: &RunOpts, steps: mpsc::UnboundedSender<Event>, stop: Arc<Notify>) -> Outcome {
    let Some(_slot) = slots().acquire(&stop).await else {
        return Outcome::Stopped(None);
    };
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
    steps: mpsc::UnboundedSender<Event>,
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
                        Event::Result(r) => result = Some(r),
                        ev => {
                            if let Event::Init(id) = &ev { session = Some(id.clone()); }
                            let _ = steps.send(ev);
                        }
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
                Event::Tool {
                    name: "Bash".into(),
                    input: "echo hi there".into()
                },
                Event::Tool {
                    name: "Read".into(),
                    input: "/a/b.rs".into()
                }
            ]
        );
        assert_eq!(
            parse_line(
                r#"{"type":"assistant","message":{"content":[{"type":"text","text":"done"}]}}"#
            ),
            vec![Event::Text("done".into())]
        );
        let res = r#"{"type":"result","subtype":"success","is_error":false,"result":"hi","session_id":"s1","total_cost_usd":0.0075,"duration_ms":1200,"usage":{"input_tokens":2,"cache_read_input_tokens":10,"cache_creation_input_tokens":5,"output_tokens":7}}"#;
        assert_eq!(
            parse_line(res),
            vec![Event::Result(RunResult {
                text: "hi".into(),
                session_id: Some("s1".into()),
                cost_usd: 0.0075,
                is_error: false,
                errors: vec![],
                input_tokens: 17,
                output_tokens: 7,
                duration_ms: 1200,
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
        assert!(!is_stale_text("invalid session token for MCP server"));
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
        assert!(t.elapsed() < Duration::from_secs(10), "{:?}", t.elapsed()); // << the 15s timeout
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
        let mut o = o;
        o.prompt = "a\0b".into();
        o.system_prompt = "s\0".into();
        let a = build_args(&o);
        assert!(a.iter().all(|x| !x.contains('\0')));
        assert_eq!(a.last().unwrap(), "ab");
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
        assert_eq!(rx.recv().await.unwrap(), Event::Init("s9".into()));
        assert_eq!(
            rx.recv().await.unwrap(),
            Event::Tool {
                name: "Bash".into(),
                input: "ls".into()
            }
        );

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

    #[tokio::test]
    async fn run_slots_cap_queue_and_stop_while_waiting() {
        let slots = Arc::new(RunSlots::new(1));
        let never = Notify::new();
        assert_eq!(slots.queued_ahead(), None);
        let first = slots.acquire(&never).await.unwrap();
        assert_eq!(slots.queued_ahead(), Some(0));
        let s2 = slots.clone();
        let waiter = tokio::spawn(async move {
            let n = Notify::new();
            s2.acquire(&n).await.is_some()
        });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(slots.queued_ahead(), Some(1));
        // a stop while waiting gives up the place in line
        let stop = Notify::new();
        stop.notify_one();
        assert!(slots.acquire(&stop).await.is_none());
        drop(first);
        assert!(waiter.await.unwrap());
        assert_eq!(slots.queued_ahead(), None);
    }
}
