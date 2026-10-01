//! 以真实 SDK/插件进程和内存宿主回调验证取消、状态及告警，不连接上游或数据库。
use gateway_plugin_sdk::{
    CallContext, Frame, Handshake, Manifest, Message, Stage,
    client::{read_frame, write_frame},
};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    process::{Child, ChildStdin, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

struct Host {
    child: Child,
    input: ChildStdin,
    frames: mpsc::Receiver<Frame>,
    records: BTreeMap<String, (Value, u64)>,
    conflicts: usize,
    revision: u64,
    model_calls: usize,
}

impl Drop for Host {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Host {
    fn start(configuration: Value) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_model-trace"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let input = child.stdin.take().unwrap();
        let mut output = child.stdout.take().unwrap();
        let (sender, frames) = mpsc::channel();
        std::thread::spawn(move || {
            loop {
                let mut header = [0; 12];
                if output.read_exact(&mut header).is_err() {
                    break;
                }
                let metadata = u32::from_be_bytes(header[..4].try_into().unwrap()) as usize;
                let payload = u64::from_be_bytes(header[4..].try_into().unwrap()) as usize;
                let mut bytes = header.to_vec();
                bytes.resize(12 + metadata + payload, 0);
                if output.read_exact(&mut bytes[12..]).is_err() {
                    break;
                }
                let frame = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap()
                    .block_on(read_frame(&mut bytes.as_slice()))
                    .unwrap();
                if sender.send(frame).is_err() {
                    break;
                }
            }
        });
        let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json")).unwrap();
        let mut host = Self {
            child,
            input,
            frames,
            records: BTreeMap::new(),
            conflicts: 0,
            revision: 0,
            model_calls: 0,
        };
        host.send(Frame::control(Message::Hello {
            handshake: Handshake {
                protocol_version: gateway_plugin_sdk::PROTOCOL_VERSION,
                artifact_sha256: "a".repeat(64),
                plugin_id: "xunzhimeng.model-trace".into(),
                instance_id: "test".into(),
                generation: 1,
                incarnation: "test".into(),
                configuration,
                contributes: manifest.contributes,
            },
        }));
        assert!(matches!(host.recv().message, Message::Ready { .. }));
        host
    }
    fn send(&mut self, frame: Frame) {
        let mut bytes = Vec::new();
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(write_frame(&mut bytes, &frame))
            .unwrap();
        self.input.write_all(&bytes).unwrap();
        self.input.flush().unwrap();
    }
    fn recv(&self) -> Frame {
        self.frames
            .recv_timeout(Duration::from_secs(10))
            .expect("插件未按时返回")
    }
    fn call(&mut self, id: u64, stage: Stage, method: &str, params: Value, payload: Value) {
        self.send(Frame {
            message: Message::Call {
                id,
                method: method.into(),
                context: CallContext {
                    call_id: id,
                    instance_id: "test".into(),
                    generation: 1,
                    incarnation: "test".into(),
                    stage,
                    timeout_ms: 5000,
                    resource_stream: false,
                    resource_scope_id: format!("scope-{id}"),
                    request_id: None,
                    attempt_id: None,
                    account_id: None,
                    credential_revision: None,
                },
                params,
            },
            payload: if payload.is_null() {
                vec![]
            } else {
                serde_json::to_vec(&payload).unwrap()
            },
        });
    }
    fn reply(&mut self, id: u64, result: Value, payload: Vec<u8>) {
        self.send(Frame {
            message: Message::Result { id, result },
            payload,
        });
    }
    fn callback(&mut self, frame: Frame) {
        let Message::Callback {
            id, method, params, ..
        } = frame.message
        else {
            panic!("expected callback")
        };
        match method.as_str() {
            "host.state.get" => {
                let key = params["key"].as_str().unwrap();
                let record = self.records.get(key).map(|(value, version)| json!({"value": value, "version": version, "schema_version": 1}));
                self.reply(id, json!({"record": record}), vec![]);
            }
            "host.state.put" => {
                let key = params["key"].as_str().unwrap().to_owned();
                if (self.conflicts > 0 && (key == "run-test" || key == "runs-index"))
                    || params["expected_version"].as_u64()
                        != self.records.get(&key).map(|(_, version)| *version)
                {
                    self.conflicts = self.conflicts.saturating_sub(1);
                    self.send(Frame::control(Message::Error {
                        id,
                        error: gateway_plugin_sdk::PluginFault::new(
                            gateway_plugin_sdk::ErrorCode::Conflict,
                            "synthetic conflict",
                        ),
                    }));
                    return;
                }
                assert!(self.records.contains_key(&key) || self.records.len() < 256);
                self.revision += 1;
                self.records
                    .insert(key, (params["value"].clone(), self.revision));
                self.reply(id, json!({"version": self.revision}), vec![]);
            }
            "host.model.execute" => {
                self.model_calls += 1;
                self.model_success(id);
            }
            _ => panic!("unexpected callback {method}"),
        }
    }
    fn finish(&mut self, id: u64) -> Frame {
        loop {
            let frame = self.recv();
            if matches!(frame.message, Message::Result { id: actual, .. } if actual == id) {
                return frame;
            }
            self.callback(frame);
        }
    }
    fn put(&mut self, key: &str, value: Value) {
        self.revision += 1;
        self.records.insert(key.into(), (value, self.revision));
    }
}
impl Host {
    fn manage(&mut self, id: u64, path: &str) {
        self.call(
            id,
            Stage::Management,
            "management.handle",
            json!({"method":"POST","path":path,"query":"","content_type":"application/json"}),
            json!({"id":"test"}),
        );
    }
    fn model_success(&mut self, id: u64) {
        use gateway_plugin_sdk::call::{
            host::ModelEventBatch,
            model::{CanonicalEvent, ExecutionEvent},
        };
        // 已通过数字数量校验，但 16KiB 落在中文字符中间。
        let text = "1,".repeat(200) + "\n" + &"测".repeat(6000);
        let bytes = ModelEventBatch {
            events: vec![ExecutionEvent::canonical(CanonicalEvent::TextDelta {
                index: 0,
                text,
            })],
        }
        .encode()
        .unwrap();
        self.reply(id, json!({"request_id":"request-test", "events":1}), bytes);
    }
    fn next_model(&mut self) -> u64 {
        loop {
            let frame = self.recv();
            if let Message::Callback { id, ref method, .. } = frame.message
                && method == "host.model.execute"
            {
                self.model_calls += 1;
                return id;
            }
            self.callback(frame);
        }
    }
}
fn sample_run() -> Value {
    json!({"id":"test","status":"pending","model":"model","client_key_id":"key",
        "account_id":null,"provider":null,"account_name":null,"created_at_ms":1,
        "updated_at_ms":now_ms(),"completed_at_ms":null,"cancel_requested":false,
        "queries":[{"index":0,"prompt":"challenge","expected_count":300,"status":"pending","attempts":[],"numbers":null,"response":null}],"result":null})
}
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}
fn response(frame: Frame) -> Value {
    assert!(matches!(frame.message, Message::Result { .. }));
    serde_json::from_slice(&frame.payload).unwrap()
}

#[test]
fn duplicate_step_and_result_cas_conflict_never_repeat_model_and_utf8_survives() {
    let mut host = Host::start(json!({}));
    host.put("run-test", sample_run());
    host.manage(1, "run/step");
    let model_id = host.next_model();
    assert_eq!(
        host.records["run-test"].0["queries"][0]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    host.manage(3, "run/step");
    assert_eq!(response(host.finish(3))["error"], "step already in flight");
    host.conflicts = 1;
    host.model_success(model_id);
    let data = response(host.finish(1));
    assert_eq!(data["run"]["status"], "collecting");
    assert_eq!(host.model_calls, 1);
    let text = host.records["run-test"].0["queries"][0]["response"]["text"]
        .as_str()
        .unwrap();
    assert!(text.len() <= 16384 && text.len() >= 16381);
    assert_eq!(
        host.records["run-test"].0["queries"][0]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn transient_failure_retries_but_cancel_conflicts_never_report_success() {
    let mut host = Host::start(json!({}));
    host.put("run-test", sample_run());
    host.manage(1, "run/step");
    let model_id = host.next_model();
    host.send(Frame::control(Message::Error {
        id: model_id,
        error: gateway_plugin_sdk::PluginFault::new(
            gateway_plugin_sdk::ErrorCode::Upstream,
            "temporary",
        ),
    }));
    assert_eq!(
        response(host.finish(1))["run"]["queries"][0]["status"],
        "pending"
    );
    host.manage(3, "run/step");
    let model_id = host.next_model();
    host.conflicts = 4;
    host.manage(5, "run/cancel");
    assert!(
        response(host.finish(5))["error"]
            .as_str()
            .unwrap()
            .contains("conflicted")
    );
    assert_eq!(host.records["run-test"].0["cancel_requested"], false);
    host.conflicts = 1;
    host.manage(7, "run/cancel");
    assert_eq!(response(host.finish(7))["run"]["status"], "cancelled");
    host.model_success(model_id);
    assert_eq!(response(host.finish(3))["run"]["status"], "cancelled");
    assert_eq!(host.model_calls, 2);
    assert_eq!(
        host.records["run-test"].0["queries"][0]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn index_conflict_is_retried_and_entry_survives() {
    let mut host = Host::start(json!({}));
    host.put("runs-index", json!({"entries":[{"id":"existing","model":"m","client_key_name":null,"account_id":null,"account_name":null,"status":"completed","created_at_ms":1,"updated_at_ms":2,"prediction":null,"probability":null}]}));
    host.conflicts = 1;
    host.call(1, Stage::Management, "management.handle",
        json!({"method":"POST","path":"runs","query":"","content_type":"application/json"}),
        json!({"model":"model","client_key_id":"key","queries":[{"prompt":"p","expected_count":300}]}));
    let data = response(host.finish(1));
    assert!(data["run"]["id"].as_str().is_some());
    let index = &host.records["runs-index"].0["entries"];
    let ids: Vec<&str> = index
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["id"].as_str())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(
        ids.contains(&"existing"),
        "index entry must survive conflict retry: {ids:?}"
    );
}

#[test]
fn cancelled_parent_attempts_remain_charged_and_recovery_is_bounded() {
    let mut host = Host::start(json!({}));
    host.put("run-test", sample_run());
    for index in 0..4 {
        let id = index * 2 + 1;
        host.manage(id, "run/step");
        host.next_model();
        host.send(Frame::control(Message::Cancel { id }));
        assert!(matches!(host.recv().message, Message::Cancelled { .. }));
        // 只推进内存测试时钟，模拟宿主已结束而页面稍后恢复。
        let mut run = host.records["run-test"].0.clone();
        run["updated_at_ms"] = json!(now_ms() - 301_000);
        host.put("run-test", run);
    }
    host.manage(9, "run/step");
    assert_eq!(response(host.finish(9))["run"]["status"], "failed");
    assert_eq!(host.model_calls, 4);
    assert_eq!(
        host.records["run-test"].0["queries"][0]["attempts"]
            .as_array()
            .unwrap()
            .len(),
        4
    );
}
