//! Single-flight, bounded IPC to the independent local Python bridge.
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc, Arc, Mutex,
    },
    time::{Duration, Instant},
};

const LIMIT: usize = 65536;
const DEADLINE: Duration = Duration::from_secs(15);
const CONTINUOUS_ROLES: [&str; 4] = ["retune", "flex", "vibrato", "humanize"];
const DISCRETE_ROLES: [&str; 2] = ["key", "scale"];

fn validate_request(value: &Value) -> Result<(), String> {
    let object = value.as_object().ok_or("Request must be an object")?;
    let op = object
        .get("op")
        .and_then(Value::as_str)
        .ok_or("Missing operation")?;
    let fields: &[&str] = match op {
        "status" | "scan" | "disconnect" => &["op"],
        "connect" => &["op", "candidateId"],
        "options" => &["op", "candidateId", "role"],
        "clear" => &["op", "connectionId"],
        "apply" => &["op", "connectionId", "sequence", "values"],
        _ => return Err("Unknown operation".into()),
    };
    if object.len() != fields.len() || fields.iter().any(|key| !object.contains_key(*key)) {
        return Err("Unexpected or missing request fields".into());
    }
    for key in ["candidateId", "connectionId"] {
        if let Some(value) = object.get(key) {
            if !value
                .as_str()
                .is_some_and(|s| !s.is_empty() && s.len() <= 128)
            {
                return Err(format!("Invalid {key}"));
            }
        }
    }
    if op == "apply" {
        if !value["sequence"]
            .as_u64()
            .is_some_and(|n| n > 0 && n <= 9_007_199_254_740_991)
        {
            return Err("sequence must be a positive JS safe integer".into());
        }
        let values = value["values"]
            .as_object()
            .ok_or("values must be an object")?;
        // Continuous roles carry normalized numbers; key/scale carry a profile
        // option label that the bridge resolves (unknown labels fail closed there).
        if values.is_empty()
            || values.len() > CONTINUOUS_ROLES.len() + DISCRETE_ROLES.len()
            || values.iter().any(|(role, value)| {
                if CONTINUOUS_ROLES.contains(&role.as_str()) {
                    !value
                        .as_f64()
                        .is_some_and(|v| v.is_finite() && (0.0..=1.0).contains(&v))
                } else if DISCRETE_ROLES.contains(&role.as_str()) {
                    !value.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 32)
                } else {
                    true
                }
            })
        {
            return Err("Only normalized continuous roles and key/scale option labels are supported".into());
        }
    }
    if op == "options"
        && !value["role"]
            .as_str()
            .is_some_and(|role| role == "key" || role == "scale")
    {
        return Err("Only key and scale options are readable".into());
    }
    if value.to_string().len() >= LIMIT {
        return Err("Request exceeds size limit".into());
    }
    Ok(())
}

fn state(error: Option<&str>) -> Value {
    json!({"phase":if error.is_some() {"error"} else {"disconnected"},
        "connectionId":null,"target":null,"capabilities":[],"instanceCount":0,
        "delivery":null,"error":error,"audioVerified":false})
}

fn failure(error: &str) -> Value {
    json!({"ok":false,"error":error,"state":state(Some(error))})
}

fn validate_response(value: &Value, op: &str) -> Result<(), String> {
    let object = value.as_object().ok_or("Worker response must be an object")?;
    let ok = object.get("ok").and_then(Value::as_bool).ok_or("Worker response ok must be boolean")?;
    if !ok && !object.get("error").and_then(Value::as_str).is_some_and(|s| !s.is_empty()) {
        return Err("Worker failure must include an error".into());
    }
    let state = object.get("state").and_then(Value::as_object).ok_or("Worker state missing")?;
    let phase = state.get("phase").and_then(Value::as_str).ok_or("Worker state phase missing")?;
    if !["disconnected", "awaiting", "ready", "error"].contains(&phase) {
        return Err("Invalid worker state phase".into());
    }
    let connection = state.get("connectionId").ok_or("Worker connectionId missing")?;
    let target = state.get("target").ok_or("Worker target missing")?;
    let capabilities = state.get("capabilities").and_then(Value::as_array).ok_or("Worker capabilities missing")?;
    if capabilities.iter().any(|v| !v.as_str().is_some_and(|s| CONTINUOUS_ROLES.contains(&s) || DISCRETE_ROLES.contains(&s))) {
        return Err("Invalid worker capabilities".into());
    }
    let count = state.get("instanceCount").and_then(Value::as_u64).ok_or("Worker instanceCount missing")?;
    let delivery = state.get("delivery").ok_or("Worker delivery missing")?;
    let error_ok = state.get("error").is_some_and(|v| v.is_null() || v.is_string());
    if !error_ok || state.get("audioVerified") != Some(&Value::Bool(false)) {
        return Err("Invalid worker error/audio state".into());
    }
    if phase == "awaiting" || phase == "ready" {
            if !connection.as_str().is_some_and(|s| !s.is_empty() && s.len() <= 128) || !target.is_object() || count == 0 && phase == "ready" {
                return Err("Connected state is incomplete".into());
            }
            let target = target.as_object().unwrap();
            if !target.get("pid").and_then(Value::as_u64).is_some_and(|n| n > 0)
                || !target.get("processName").is_some_and(Value::is_string)
                || !target.get("pluginName").is_some_and(Value::is_string)
                || !target.get("profileId").is_some_and(Value::is_string) {
                return Err("Connected target is incomplete".into());
            }
    } else {
            if !connection.is_null() || !target.is_null() || !delivery.is_null() || !capabilities.is_empty() || count != 0 {
                return Err("Disconnected/error state contains live connection data".into());
            }
    }
    if let Some(delivery) = delivery.as_object() {
        if !["cached","submitted"].contains(&delivery.get("stage").and_then(Value::as_str).unwrap_or(""))
            || !delivery.get("sequence").and_then(Value::as_u64).is_some_and(|n| n > 0 && n <= 9_007_199_254_740_991) {
            return Err("Invalid delivery state".into());
        }
    } else if !delivery.is_null() {
        return Err("Invalid delivery state".into());
    }
    // A rejected operation can carry a valid replacement session. Validate that
    // entire state before returning, but do not require a success-only payload.
    if !ok {
        return Ok(());
    }
    if op == "options" {
        let options = object
            .get("options")
            .and_then(Value::as_object)
            .ok_or("Options result missing")?;
        if options.get("ok") != Some(&Value::Bool(true))
            || options.get("source").and_then(Value::as_str) != Some("profile")
            || !options
                .get("profile_id")
                .and_then(Value::as_str)
                .is_some_and(|value| !value.is_empty())
            || !options
                .get("role")
                .and_then(Value::as_str)
                .is_some_and(|role| role == "key" || role == "scale")
            || !options
                .get("id")
                .and_then(Value::as_u64)
                .is_some_and(|id| id <= u32::MAX as u64)
        {
            return Err("Invalid profile options metadata".into());
        }
        let values = options
            .get("options")
            .and_then(Value::as_array)
            .ok_or("Profile options missing")?;
        if values.is_empty()
            || values.iter().any(|item| {
                let Some(item) = item.as_object() else { return true };
                !item
                    .get("label")
                    .and_then(Value::as_str)
                    .is_some_and(|label| !label.is_empty())
                    || !item
                        .get("normalized")
                        .and_then(Value::as_f64)
                        .is_some_and(|value| value.is_finite() && (0.0..=1.0).contains(&value))
            })
        {
            return Err("Invalid profile option values".into());
        }
    }
    if op == "clear"
        && (object.get("cleared") != Some(&Value::Bool(true))
            || object.get("dspReset") != Some(&Value::Bool(false)))
    {
        return Err("Invalid clear acknowledgement".into());
    }
    if op == "scan" {
        let candidates = object.get("candidates").and_then(Value::as_array).ok_or("Scan candidates missing")?;
        if candidates.iter().any(|candidate| {
            let Some(candidate) = candidate.as_object() else { return true };
            !candidate.get("candidateId").is_some_and(|v| v.as_str().is_some_and(|s| !s.is_empty()))
                || !candidate.get("pid").and_then(Value::as_u64).is_some_and(|n| n > 0)
                || !candidate.get("processName").is_some_and(Value::is_string)
                || !candidate.get("pluginName").is_some_and(Value::is_string)
                || !candidate.get("profileId").is_some_and(|v| v.is_null() || v.is_string())
                || !candidate.get("compatible").is_some_and(Value::is_boolean)
                || !candidate.get("reason").is_some_and(|v| v.is_null() || v.is_string())
        }) || !object.get("skippedCount").and_then(Value::as_u64).is_some() {
            return Err("Invalid scan result".into());
        }
    }
    Ok(())
}

struct Worker {
    child: Arc<Mutex<Child>>,
    requests: mpsc::SyncSender<String>,
    responses: mpsc::Receiver<Result<String, String>>,
    io: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    fn spawn(mut command: Command) -> Result<Self, String> {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x08000000); // CREATE_NO_WINDOW
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("Cannot start Python worker: {e}"))?;
        let mut input = child.stdin.take().ok_or("Missing worker stdin")?;
        let mut output = BufReader::new(child.stdout.take().ok_or("Missing worker stdout")?);
        let (requests, incoming) = mpsc::sync_channel::<String>(1);
        let (outgoing, responses) = mpsc::sync_channel(1);
        let io = std::thread::spawn(move || {
            while let Ok(request) = incoming.recv() {
                let result = (|| {
                    input
                        .write_all(request.as_bytes())
                        .map_err(|e| e.to_string())?;
                    input.write_all(b"\n").map_err(|e| e.to_string())?;
                    input.flush().map_err(|e| e.to_string())?;
                    let mut bytes = Vec::new();
                    output
                        .by_ref()
                        .take((LIMIT + 1) as u64)
                        .read_until(b'\n', &mut bytes)
                        .map_err(|e| e.to_string())?;
                    if bytes.len() > LIMIT || bytes.last() != Some(&b'\n') {
                        return Err("Worker EOF or oversized/incomplete response".into());
                    }
                    bytes.pop();
                    if bytes.last() == Some(&b'\r') {
                        bytes.pop();
                    }
                    String::from_utf8(bytes).map_err(|e| e.to_string())
                })();
                let failed = result.is_err();
                if outgoing.send(result).is_err() || failed {
                    break;
                }
            }
        });
        Ok(Self {
            child: Arc::new(Mutex::new(child)),
            requests,
            responses,
            io: Some(io),
        })
    }

    fn exchange(&mut self, request: &str, deadline: Instant) -> Result<String, String> {
        if request.len() >= LIMIT {
            return Err("Request exceeds size limit".into());
        }
        if self
            .child
            .lock()
            .map_err(|_| "Worker lock poisoned")?
            .try_wait()
            .map_err(|e| e.to_string())?
            .is_some()
        {
            return Err("Worker exited; scan and explicitly reconnect".into());
        }
        self.requests
            .try_send(request.to_owned())
            .map_err(|_| "Worker IPC unavailable")?;
        self.responses
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .map_err(|_| "Worker deadline exceeded or IPC closed".to_owned())?
    }

    fn stop(&mut self) {
        kill_child(&self.child);
        // Drop the request sender before joining, so an idle reader exits as well.
        let (replacement, _) = mpsc::sync_channel(1);
        drop(std::mem::replace(&mut self.requests, replacement));
        if let Some(io) = self.io.take() {
            let _ = io.join();
        }
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stop();
    }
}

fn kill_child(child: &Arc<Mutex<Child>>) {
    if let Ok(mut child) = child.lock() {
        if child.try_wait().ok().flatten().is_none() {
            let _ = child.kill();
        }
        let _ = child.wait();
    }
}

#[derive(Default)]
struct Manager {
    worker: Option<Worker>,
}

#[derive(Default)]
struct Inner {
    manager: Mutex<Manager>,
    child: Mutex<Option<Arc<Mutex<Child>>>>,
    stopped: AtomicBool,
}

#[derive(Clone, Default)]
pub struct AutotuneState {
    inner: Arc<Inner>,
}

impl AutotuneState {
    fn kill(&self) {
        if let Ok(child) = self.inner.child.lock() {
            if let Some(child) = child.as_ref() {
                kill_child(child);
            }
        }
    }

    pub fn stop(&self) {
        self.inner.stopped.store(true, Ordering::Release);
        self.kill();
    }

    fn execute(&self, request: Value, deadline: Instant) -> Value {
        let mut manager = loop {
            match self.inner.manager.try_lock() {
                Ok(manager) => break manager,
                Err(std::sync::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                _ => {
                    self.kill();
                    return failure("Worker queue deadline exceeded");
                }
            }
        };
        let result = (|| -> Result<Value, String> {
            if self.inner.stopped.load(Ordering::Acquire) {
                return Err("Application is closing".into());
            }
            if Instant::now() >= deadline {
                return Err("Worker request deadline exceeded".into());
            }
            validate_request(&request)?;
            if manager.worker.is_none() {
                match request["op"].as_str() {
                    Some("status" | "disconnect") => {
                        return Ok(json!({"ok":true,"state":state(None)}))
                    }
                    Some("scan" | "options") => {}
                    _ => return Err("No worker connection; scan and explicitly connect".into()),
                }
                let reference = std::env::var_os("TUNE_LOVE_REFERENCE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| {
                        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../reference")
                    });
                let script = reference
                    .join("app_bridge.py")
                    .canonicalize()
                    .map_err(|e| format!("Cannot locate reference/app_bridge.py: {e}"))?;
                let mut command = Command::new(
                    std::env::var_os("TUNE_LOVE_PYTHON").unwrap_or_else(|| "python".into()),
                );
                command.arg("-u").arg(script);
                let worker = Worker::spawn(command)?;
                *self
                    .inner
                    .child
                    .lock()
                    .map_err(|_| "Worker ownership lock poisoned")? = Some(worker.child.clone());
                manager.worker = Some(worker);
                if self.inner.stopped.load(Ordering::Acquire) {
                    return Err("Application is closing".into());
                }
            }
            let response = manager
                .worker
                .as_mut()
                .unwrap()
                .exchange(&request.to_string(), deadline)?;
            let parsed: Value =
                serde_json::from_str(&response).map_err(|_| "Malformed worker response")?;
            validate_response(&parsed, request["op"].as_str().unwrap_or(""))?;
            Ok(parsed)
        })();
        match result {
            Ok(response) => response,
            Err(error) => {
                manager.worker.take(); // kill/reap before any future request; never retry a write
                if let Ok(mut child) = self.inner.child.lock() {
                    *child = None;
                }
                failure(&error)
            }
        }
    }
}

#[tauri::command]
pub async fn autotune_command(
    request: Value,
    state: tauri::State<'_, AutotuneState>,
) -> Result<Value, String> {
    let shared = state.inner().clone();
    let deadline = Instant::now() + DEADLINE;
    Ok(
        tauri::async_runtime::spawn_blocking(move || shared.execute(request, deadline))
            .await
            .unwrap_or_else(|_| failure("Worker task failed")),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn failed_operations_require_full_state_but_not_success_payloads() {
        for op in ["scan", "options", "clear"] {
            let mut response = ready_response();
            response["ok"] = json!(false);
            response["error"] = json!("Request rejected; live session retained");
            assert!(validate_response(&response, op).is_ok(), "{op}");
            response["state"] = json!({"audioVerified": false});
            assert!(validate_response(&response, op).is_err(), "{op}: incomplete state accepted");
        }
        let mut response = ready_response();
        response["ok"] = json!(false);
        assert!(validate_response(&response, "status").is_err(), "missing error accepted");
    }

    fn ready_response() -> Value {
        json!({"ok":true,"state":{"phase":"ready","connectionId":"new",
            "target":{"pid":1234,"processName":"fixture.exe","pluginName":"Fixture.vst3","profileId":"fixture"},
            "capabilities":["retune","flex"],"instanceCount":2,"delivery":{"stage":"cached","sequence":1},
            "error":null,"audioVerified":false}})
    }

    #[test]
    fn response_validator_rejects_malformed_state_and_scan() {
        let valid = ready_response();
        assert!(validate_response(&valid, "status").is_ok());
        for (field, value) in [
            ("phase", json!("unknown")), ("connectionId", json!(null)),
            ("target", json!({"pid":1234})), ("capabilities", json!(["pitch"])),
            ("instanceCount", json!(0)), ("instanceCount", json!(1.5)),
            ("delivery", json!({"stage":"audible","sequence":1})),
            ("delivery", json!({"stage":"cached","sequence":0})),
            ("delivery", json!({"stage":"cached","sequence":9007199254740992u64})),
            ("error", json!(false)), ("audioVerified", json!(true)),
        ] {
            let mut bad = valid.clone();
            bad["state"][field] = value;
            assert!(validate_response(&bad, "status").is_err(), "{bad}");
        }
        for field in ["phase","connectionId","target","capabilities","instanceCount","delivery","error","audioVerified"] {
            let mut bad = valid.clone();
            bad["state"].as_object_mut().unwrap().remove(field);
            assert!(validate_response(&bad, "status").is_err(), "{field}");
        }
        for phase in ["disconnected", "error"] {
            let mut bad = valid.clone();
            bad["state"]["phase"] = json!(phase);
            assert!(validate_response(&bad, "status").is_err());
        }
        let candidate = json!({"candidateId":"candidate","pid":1234,"processName":"fixture.exe",
            "pluginName":"Fixture.vst3","profileId":"fixture","compatible":true,"reason":null});
        let mut scan = json!({"ok":true,"state":state(None),"candidates":[candidate],"skippedCount":0});
        assert!(validate_response(&scan, "scan").is_ok());
        scan["candidates"][0]["compatible"] = json!("yes");
        assert!(validate_response(&scan, "scan").is_err());
        scan["candidates"] = json!([]);
        scan["skippedCount"] = json!(-1);
        assert!(validate_response(&scan, "scan").is_err());
        assert!(validate_response(&json!({"ok":true,"state":state(None)}), "scan").is_err());
        let mut scan_failure = valid;
        scan_failure["ok"] = json!(false);
        scan_failure["error"] = json!("Scan failed");
        scan_failure["state"]["error"] = json!("Scan failed");
        assert!(validate_response(&scan_failure, "scan").is_ok());
    }

    #[test]
    fn validates_read_only_options_and_explicit_clear_contracts() {
        assert!(validate_request(&json!({
            "op":"options", "candidateId":"candidate", "role":"key"
        })).is_ok());
        assert!(validate_request(&json!({
            "op":"clear", "connectionId":"connection"
        })).is_ok());

        let options = json!({
            "ok":true,
            "state":state(None),
            "options":{
                "ok":true, "source":"profile", "profile_id":"fixture",
                "role":"key", "id":2,
                "options":[{"label":"C", "normalized":0.0}, {"label":"B", "normalized":1.0}]
            }
        });
        assert!(validate_response(&options, "options").is_ok());

        let mut clear = ready_response();
        clear["state"]["delivery"] = Value::Null;
        clear["cleared"] = json!(true);
        clear["dspReset"] = json!(false);
        assert!(validate_response(&clear, "clear").is_ok());
    }

    #[test]
    fn maximum_request_to_nonreading_worker_times_out_and_reaps() {
        let mut manager = Manager { worker: Some(Worker::spawn(python("import time; time.sleep(4)")).unwrap()) };
        let child = manager.worker.as_ref().unwrap().child.clone();
        let start = Instant::now();
        let result = manager.worker.as_mut().unwrap().exchange(&"x".repeat(65535),
            start + Duration::from_millis(150));
        assert!(result.is_err());
        manager.worker.take();
        assert!(start.elapsed() < Duration::from_secs(5), "blocked stdin prevented deadline cleanup");
        assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    }

    #[test]
    fn manager_invalid_response_fails_closed_and_reaps() {
        let shared = AutotuneState::default();
        let worker = Worker::spawn(python("import sys\nfor line in sys.stdin: print('{\"ok\":true,\"state\":{\"audioVerified\":false}}', flush=True)")).unwrap();
        let child = worker.child.clone();
        *shared.inner.child.lock().unwrap() = Some(child.clone());
        shared.inner.manager.lock().unwrap().worker = Some(worker);
        let result = shared.execute(json!({"op":"status"}), Instant::now()+Duration::from_secs(3));
        assert_eq!(result["ok"], false);
        assert_eq!(result["state"]["phase"], "error");
        assert!(shared.inner.manager.lock().unwrap().worker.is_none());
        assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    }
    #[test]
    fn rejects_unrestricted_fields_and_bad_values() {
        for value in [
            serde_json::json!({"op":"scan","path":"arbitrary"}),
            serde_json::json!({"op":"apply","connectionId":"x","sequence":1,"values":{"key":0.2}}),
            serde_json::json!({"op":"apply","connectionId":"x","sequence":0,"values":{"retune":0.2}}),
            serde_json::json!({"op":"apply","connectionId":"x","sequence":9007199254740992u64,"values":{"retune":0.2}}),
            serde_json::json!({"op":"apply","connectionId":"x","sequence":1,"values":{}}),
            serde_json::json!({"op":"apply","connectionId":"x","sequence":1,"values":{"retune":1.2}}),
            serde_json::json!({"op":"apply","connectionId":"x","sequence":1,"values":{"key":""}}),
            serde_json::json!({"op":"apply","connectionId":"x","sequence":1,"values":{"scale":5}}),
            serde_json::json!({"op":"apply","connectionId":"x","sequence":1,"values":{"key":"x".repeat(33)}}),
        ] {
            assert!(validate_request(&value).is_err(), "{value}");
        }
        assert!(validate_request(
            &serde_json::json!({"op":"apply","connectionId":"x","sequence":2,
            "values":{"retune":0.2,"vibrato":1.0}})
        )
        .is_ok());
        assert!(validate_request(
            &serde_json::json!({"op":"apply","connectionId":"x","sequence":3,
            "values":{"retune":0.2,"key":"F#","scale":"Minor"}})
        )
        .is_ok());
        let mut capable = ready_response();
        capable["state"]["capabilities"] = json!(["retune","key","scale"]);
        assert!(validate_response(&capable, "status").is_ok());
    }

    fn python(script: &str) -> Command {
        let mut command = Command::new(
            std::env::var_os("TUNE_LOVE_PYTHON").unwrap_or_else(|| "python".into()),
        );
        command.args(["-u", "-c", script]);
        command
    }

    #[test]
    fn worker_timeout_is_bounded_and_reaped() {
        let mut worker = Worker::spawn(python("import time; time.sleep(60)")).unwrap();
        let start = Instant::now();
        assert!(worker
            .exchange("{}", start + Duration::from_millis(100))
            .is_err());
        worker.stop();
        assert!(start.elapsed() < Duration::from_secs(5));
        assert!(worker.child.lock().unwrap().try_wait().unwrap().is_some());
    }

    #[test]
    fn worker_eof_is_an_error() {
        let mut worker = Worker::spawn(python("pass")).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        assert!(worker.exchange("{}", deadline).is_err());
        worker.stop();
        assert!(worker.child.lock().unwrap().try_wait().unwrap().is_some());
    }

    #[test]
    fn worker_reuses_process_and_rejects_oversized_response() {
        let mut worker = Worker::spawn(python(
            "import sys\nfor line in sys.stdin: print(line.strip(), flush=True)",
        ))
        .unwrap();
        for message in ["{}", "{\"ok\":true}"] {
            assert_eq!(
                worker
                    .exchange(message, Instant::now() + Duration::from_secs(5))
                    .unwrap(),
                message
            );
        }
        worker.stop();
        let mut huge = Worker::spawn(python("print('x'*70000, flush=True)")).unwrap();
        assert!(huge
            .exchange("{}", Instant::now() + Duration::from_secs(5))
            .is_err());
        huge.stop();
    }

    #[test]
    fn manager_loses_connection_on_eof_and_never_starts_worker_for_apply() {
        let shared = AutotuneState::default();
        let initial = shared.execute(json!({"op":"status"}), Instant::now() + DEADLINE);
        assert_eq!(initial["state"]["phase"], "disconnected");
        let apply = json!({"op":"apply","connectionId":"old","sequence":1,"values":{"retune":0.4}});
        let failed = shared.execute(apply, Instant::now() + DEADLINE);
        assert_eq!(failed["ok"], false);
        assert!(shared.inner.manager.lock().unwrap().worker.is_none());
        let worker = Worker::spawn(python("pass")).unwrap();
        let child = worker.child.clone();
        *shared.inner.child.lock().unwrap() = Some(child.clone());
        shared.inner.manager.lock().unwrap().worker = Some(worker);
        let failed = shared.execute(json!({"op":"status"}), Instant::now() + DEADLINE);
        assert_eq!(failed["ok"], false);
        assert_eq!(failed["state"]["phase"], "error");
        assert!(failed["state"]["connectionId"].is_null());
        assert!(child.lock().unwrap().try_wait().unwrap().is_some());
    }

    #[test]
    fn application_stop_kills_own_worker_and_prevents_restart() {
        let shared = AutotuneState::default();
        let worker = Worker::spawn(python("import time; time.sleep(60)")).unwrap();
        let child = worker.child.clone();
        *shared.inner.child.lock().unwrap() = Some(child.clone());
        shared.inner.manager.lock().unwrap().worker = Some(worker);
        shared.stop();
        assert!(child.lock().unwrap().try_wait().unwrap().is_some());
        let result = shared.execute(json!({"op":"scan"}), Instant::now() + DEADLINE);
        assert_eq!(result["ok"], false);
        assert!(shared.inner.manager.lock().unwrap().worker.is_none());
    }
}
