//! ModelTrace 模型指纹检测插件：管理页面 + 分步执行的三条数字指纹挑战。

mod model;
mod parse;
mod runs;
mod settings;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use gateway_plugin_sdk::{
    ErrorCode, PluginFault,
    call::{
        host::{StateDeleteRequest, StateDeleteResult},
        management::{ManagementPage, ManagementRegistration, ManagementResource, ManagementRoute},
    },
    client::{HostClient, PluginBuilder, SessionConfig, TypedCall, TypedReply},
};
use serde::Deserialize;
use serde_json::json;

use crate::runs::{AttemptRecord, QueryState, RunIndexEntry, RunState};

/// 挑战长度约定与指纹算法一致：1–355 的整数序列，每题 100–400 个。
const MIN_EXPECTED_COUNT: u32 = 100;
const MAX_EXPECTED_COUNT: u32 = 400;
const MAX_QUERIES: usize = 3;
const MAX_PROMPT_BYTES: usize = 8 * 1024;
/// 回答文本短预览长度；完整序列按数字保存，不回显原始输出。
const TEXT_PREVIEW_CHARS: usize = 240;
/// 原始输出持久化上限，超出截断保留前缀。
const RESPONSE_TEXT_BYTES: usize = 16 * 1024;
/// 状态写入冲突时的最大重试轮数（步骤调用 + 结果落盘两段 CAS）。
const MAX_CAS_RETRIES: u32 = 4;

struct App {
    /// 运行 ID 单调计数；配合毫秒时间戳避免重启冲突。
    counter: AtomicU64,
}

#[tokio::main]
async fn main() {
    let session = match gateway_plugin_sdk::client::PluginSession::accept(
        tokio::io::stdin(),
        tokio::io::stdout(),
        SessionConfig::default(),
    )
    .await
    {
        Ok(session) => session,
        Err(_) => return,
    };
    let app = Arc::new(App {
        counter: AtomicU64::new(0),
    });
    let plugin = match PluginBuilder::from_json(include_bytes!("../plugin.json"))
        .and_then(|builder| builder.management(registration(), management(app.clone())))
        .and_then(|builder| builder.build())
    {
        Ok(plugin) => plugin,
        Err(_) => return,
    };
    let _ = session.run(plugin).await;
}

type BoxFuture<T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send>>;
type ManagementCall = TypedCall<gateway_plugin_sdk::call::management::ManagementRequest>;
type ManagementResponse = gateway_plugin_sdk::call::management::ManagementResponse;
type ManagementResult = Result<TypedReply<ManagementResponse>, PluginFault>;

fn management(app: Arc<App>) -> impl Fn(ManagementCall) -> BoxFuture<ManagementResult> {
    move |call: ManagementCall| {
        let app = Arc::clone(&app);
        Box::pin(async move { handle(&app, call).await })
    }
}

async fn handle(app: &App, call: ManagementCall) -> ManagementResult {
    let request = &call.request;
    match (request.method.as_str(), request.path.as_str()) {
        ("GET", "bootstrap") => bootstrap(&call.host).await,
        ("GET", "models") => models(&call, &request.query).await,
        ("GET", "settings") => settings::load(&call.host).await,
        ("POST", "settings") => settings::save(&call).await,
        ("POST", "settings-reset") => settings::reset(&call.host).await,
        ("POST", "runs") => create_run(app, &call).await,
        ("GET", "runs") => list_runs(&call.host).await,
        ("POST", "run/step") => step_run(&call).await,
        ("POST", "run/cancel") => cancel_run(&call).await,
        ("GET", "run") => get_run(&call, &request.query).await,
        ("POST", "run/report") => report_run(&call).await,
        ("POST", "run/delete") => delete_run(&call).await,
        _ => json_response(404, json!({"error": "unknown management route"})),
    }
}

fn json_response(status: u16, body: serde_json::Value) -> ManagementResult {
    Ok(TypedReply::new(ManagementResponse {
        status,
        content_type: "application/json".to_owned(),
        headers: Vec::new(),
    })
    .with_payload(serde_json::to_vec(&body).unwrap_or_default()))
}

fn decode_body<T: serde::de::DeserializeOwned>(call: &ManagementCall) -> Result<T, PluginFault> {
    serde_json::from_slice(&call.payload)
        .map_err(|_| PluginFault::new(ErrorCode::InvalidInput, "request body is malformed"))
}

fn query_param(query: &str, key: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if parts.next() == Some(key) {
            return Some(percent_decode(parts.next().unwrap_or_default()));
        }
    }
    None
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' if index + 2 < bytes.len() => {
                if let Ok(value) = u8::from_str_radix(&input[index + 1..index + 3], 16) {
                    output.push(value);
                } else {
                    output.push(b'%');
                }
                index += 3;
            }
            b'+' => {
                output.push(b' ');
                index += 1;
            }
            byte => {
                output.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn code_name(code: ErrorCode) -> &'static str {
    match code {
        ErrorCode::PermissionDenied => "permission_denied",
        ErrorCode::InvalidInput => "invalid_input",
        ErrorCode::Timeout => "timeout",
        ErrorCode::Cancelled => "cancelled",
        ErrorCode::Capacity => "capacity",
        ErrorCode::Upstream => "upstream",
        ErrorCode::Rejected => "rejected",
        ErrorCode::Unsupported => "unsupported",
        ErrorCode::Conflict => "conflict",
        _ => "fault",
    }
}

fn fault_message(fault: &PluginFault) -> String {
    format!("{}: {}", code_name(fault.code), fault.message)
}

async fn bootstrap(host: &HostClient) -> ManagementResult {
    let keys = match model::list_keys(host).await {
        Ok(keys) => keys,
        Err(error) => return json_response(502, json!({"error": fault_message(&error)})),
    };
    let accounts = match model::list_accounts(host).await {
        Ok(accounts) => accounts,
        Err(error) => return json_response(502, json!({"error": fault_message(&error)})),
    };
    json_response(
        200,
        json!({
            "keys": keys.iter().map(|key| json!({
                "id": key.id, "name": key.name, "enabled": key.enabled,
            })).collect::<Vec<_>>(),
            "accounts": accounts.iter().map(|account| json!({
                "account_id": account.account_id,
                "provider_id": account.provider_id,
                "name": account.name,
                "email": account.email,
                "upstream_user_id": account.upstream_user_id,
                "enabled": account.enabled,
            })).collect::<Vec<_>>(),
        }),
    )
}

async fn models(call: &ManagementCall, query: &str) -> ManagementResult {
    let Some(key_id) = query_param(query, "key").filter(|key| !key.is_empty()) else {
        return json_response(400, json!({"error": "missing key"}));
    };
    match model::list_models(&call.host, &key_id).await {
        Ok(models) => json_response(200, json!({"models": models})),
        Err(error) => json_response(502, json!({"error": fault_message(&error)})),
    }
}

/// 允许透传的推理强度；`auto` 在页面上表示不写字段。
const REASONING_EFFORTS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

#[derive(Debug, Deserialize)]
struct CreateRequest {
    model: String,
    client_key_id: String,
    #[serde(default)]
    client_key_name: Option<String>,
    #[serde(default)]
    reasoning_effort: Option<String>,
    #[serde(default)]
    account_id: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    account_name: Option<String>,
    queries: Vec<CreateQuery>,
}

#[derive(Debug, Deserialize)]
struct CreateQuery {
    prompt: String,
    expected_count: u32,
}

fn next_run_id(app: &App) -> String {
    let counter = app.counter.fetch_add(1, Ordering::Relaxed);
    let time = runs::now_ms();
    format!("{time:x}-{counter:04x}")
}

/// 运行进入终态或需要归因时同步索引，历史列表才能反映真实状态与结果。
async fn sync_index(host: &HostClient, run: &runs::RunState) {
    runs::touch_index(host, run.index_entry()).await;
}

fn is_terminal(status: &str) -> bool {
    matches!(status, "completed" | "cancelled" | "failed")
}

/// 不可重试的模型调用错误：权限、策略拒绝或协议不支持，继续尝试不会好转。
/// `Capacity`（含额度耗尽/限流）保留有界重试，最后一次失败会终止运行。
fn is_permanent(fault: &PluginFault) -> bool {
    match fault.code {
        ErrorCode::PermissionDenied
        | ErrorCode::Rejected
        | ErrorCode::InvalidInput
        | ErrorCode::Unsupported => true,
        // 上游 402/403/429 都是额度/计费类信号；419/425 等保留给重试路径。
        _ => matches!(fault.http_status, Some(402 | 403 | 429)),
    }
}

/// 终态说明写入运行与索引，让关页面后的真实原因可直接在历史里看到。
fn finish_run(run: &mut runs::RunState, status: &str, note: Option<String>) {
    run.status = status.to_owned();
    run.status_note = note;
    run.completed_at_ms = Some(runs::now_ms());
    run.updated_at_ms = runs::now_ms();
}

/// 页面关闭后没有驱动方：读取时把超时未更新的非终态运行收敛为中断。
/// `interrupted` 不是终态，历史里的"继续"可以从断点恢复；`collecting`
/// 依赖页面本地归因，同样保留给继续流程。
async fn settle_stale(
    host: &HostClient,
    run: &mut runs::RunState,
    version: u64,
) -> Result<u64, PluginFault> {
    let stale = !is_terminal(&run.status)
        && run.status != "interrupted"
        && run.status != "collecting"
        && runs::now_ms().saturating_sub(run.updated_at_ms) > runs::STALE_RUN_MS;
    if !stale {
        return Ok(version);
    }
    let mut settled = run.clone();
    for query in &mut settled.queries {
        if query.status == "running" {
            // 旧版未预登记的调用也补记一次未知消耗，恢复不能绕开尝试预算。
            if query
                .attempts
                .last()
                .is_none_or(|attempt| attempt.status != "running")
            {
                query
                    .attempts
                    .push(running_attempt(query.attempts.len() as u32 + 1));
            }
            if let Some(attempt) = query.attempts.last_mut() {
                attempt.status = "aborted".to_owned();
                attempt.error = Some("步骤中断，调用结果未确认".to_owned());
            }
            query.status = "pending".to_owned();
        }
    }
    if settled.cancel_requested {
        finish_run(&mut settled, "cancelled", Some("检测已取消".to_owned()));
    } else {
        settled.status = "interrupted".to_owned();
        settled.status_note = Some("检测中断：页面关闭或步骤超时，可继续".to_owned());
        settled.updated_at_ms = runs::now_ms();
    }
    let version = runs::save_run(host, &settled, Some(version)).await?;
    *run = settled;
    sync_index(host, run).await;
    Ok(version)
}

async fn create_run(app: &App, call: &ManagementCall) -> ManagementResult {
    let request: CreateRequest = match decode_body(call) {
        Ok(request) => request,
        Err(error) => return json_response(400, json!({"error": error.message})),
    };
    if request.model.is_empty() || request.model.len() > 128 {
        return json_response(400, json!({"error": "model is required"}));
    }
    if request.client_key_id.is_empty() || request.client_key_id.len() > 128 {
        return json_response(400, json!({"error": "client_key_id is required"}));
    }
    if let Some(effort) = &request.reasoning_effort
        && !effort.is_empty()
        && !REASONING_EFFORTS.contains(&effort.as_str())
    {
        return json_response(
            400,
            json!({"error": "reasoning_effort must be low|medium|high|xhigh|max"}),
        );
    }
    if request.queries.is_empty() || request.queries.len() > MAX_QUERIES {
        return json_response(
            400,
            json!({"error": "queries must contain 1..=3 challenges"}),
        );
    }
    let mut seen_counts = BTreeSet::new();
    for query in &request.queries {
        if !(MIN_EXPECTED_COUNT..=MAX_EXPECTED_COUNT).contains(&query.expected_count) {
            return json_response(400, json!({"error": "expected_count out of range"}));
        }
        seen_counts.insert(query.expected_count);
        if query.prompt.is_empty() || query.prompt.len() > MAX_PROMPT_BYTES {
            return json_response(
                400,
                json!({"error": "challenge prompt is empty or too large"}),
            );
        }
    }
    // 与 ModelTrace 一致：多道挑战要求不同长度，避免同分布重复。
    if request.queries.len() > 1 && seen_counts.len() != request.queries.len() {
        return json_response(400, json!({"error": "challenge counts must be distinct"}));
    }
    let now = runs::now_ms();
    let run = RunState {
        id: next_run_id(app),
        status: "pending".to_owned(),
        model: request.model,
        client_key_id: request.client_key_id,
        client_key_name: request.client_key_name.filter(|name| !name.is_empty()),
        reasoning_effort: request.reasoning_effort.filter(|effort| !effort.is_empty()),
        account_id: request.account_id.filter(|id| !id.is_empty()),
        provider: request.provider.filter(|id| !id.is_empty()),
        account_name: request.account_name.filter(|name| !name.is_empty()),
        created_at_ms: now,
        updated_at_ms: now,
        completed_at_ms: None,
        cancel_requested: false,
        status_note: None,
        queries: request
            .queries
            .into_iter()
            .enumerate()
            .map(|(index, query)| QueryState {
                index: index as u32,
                prompt: query.prompt,
                expected_count: query.expected_count,
                status: "pending".to_owned(),
                attempts: Vec::new(),
                numbers: None,
                response: None,
            })
            .collect(),
        result: None,
    };
    let run_id = run.id.clone();
    match runs::save_run(&call.host, &run, None).await {
        Ok(_) => {
            runs::touch_index(
                &call.host,
                RunIndexEntry {
                    id: run_id.clone(),
                    model: run.model.clone(),
                    client_key_name: run.client_key_name.clone(),
                    account_id: run.account_id.clone(),
                    account_name: run.account_name.clone(),
                    status: run.status.clone(),
                    status_note: None,
                    created_at_ms: now,
                    updated_at_ms: now,
                    prediction: None,
                    probability: None,
                },
            )
            .await;
            json_response(200, json!({"id": run_id, "run": run.view()}))
        }
        Err(error) => json_response(500, json!({"error": fault_message(&error)})),
    }
}

async fn list_runs(host: &HostClient) -> ManagementResult {
    let entries = runs::load_index(host).await;
    // 索引里陈旧非终态条目直接收敛，页面打开就能看到真实状态。
    for entry in &entries {
        if is_terminal(&entry.status)
            || entry.status == "interrupted"
            || entry.status == "collecting"
        {
            continue;
        }
        let stale = entry.updated_at_ms == 0
            || runs::now_ms().saturating_sub(entry.updated_at_ms) > runs::STALE_RUN_MS;
        if !stale {
            continue;
        }
        if let Ok(Some((mut run, version))) = runs::load_run(host, &entry.id).await
            && let Err(error) = settle_stale(host, &mut run, version).await
        {
            return json_response(500, json!({"error": fault_message(&error)}));
        }
    }
    json_response(200, json!({"runs": runs::load_index(host).await}))
}

async fn get_run(call: &ManagementCall, query: &str) -> ManagementResult {
    let Some(id) = query_param(query, "id").filter(|id| !id.is_empty()) else {
        return json_response(400, json!({"error": "missing id"}));
    };
    match runs::load_run(&call.host, &id).await {
        Ok(Some((mut run, version))) => {
            if let Err(error) = settle_stale(&call.host, &mut run, version).await {
                return json_response(500, json!({"error": fault_message(&error)}));
            }
            json_response(200, json!({"run": run.view()}))
        }
        _ => json_response(404, json!({"error": "run not found"})),
    }
}

#[derive(Debug, Deserialize)]
struct RunRef {
    id: String,
    /// `report`：页面本地归因结果摘要。
    #[serde(default)]
    result: Option<serde_json::Value>,
}

/// 执行下一道待处理挑战：先 CAS 标记 running，调用模型，再 CAS 落盘尝试结果。
/// 模型响应先在页面本地解析为数字序列随本请求带回；解析失败按无效回答计入尝试。
async fn step_run(call: &ManagementCall) -> ManagementResult {
    let request: RunRef = match decode_body(call) {
        Ok(request) => request,
        Err(error) => return json_response(400, json!({"error": error.message})),
    };
    // 恢复窗口必须长于本次宿主调用期限，否则不能安全地重新占用步骤。
    if call.context.timeout_ms >= runs::STALE_RUN_MS as u64 {
        return json_response(
            400,
            json!({"error": "host timeout exceeds step recovery window"}),
        );
    }
    for _ in 0..MAX_CAS_RETRIES {
        let Some((mut run, version)) = (match runs::load_run(&call.host, &request.id).await {
            Ok(found) => found,
            Err(error) => return json_response(500, json!({"error": fault_message(&error)})),
        }) else {
            return json_response(404, json!({"error": "run not found"}));
        };
        // 过期恢复写入后必须使用新版本，不能用恢复前版本占用挑战。
        let version = match settle_stale(&call.host, &mut run, version).await {
            Ok(version) => version,
            Err(error) if error.code == ErrorCode::Conflict => continue,
            Err(error) => return json_response(500, json!({"error": fault_message(&error)})),
        };
        if is_terminal(&run.status) {
            sync_index(&call.host, &run).await;
            return json_response(
                409,
                json!({"error": "run already finished", "run": run.view()}),
            );
        }
        if run.cancel_requested {
            finish_run(&mut run, "cancelled", Some("检测已取消".to_owned()));
            match runs::save_run(&call.host, &run, Some(version)).await {
                Ok(_) => {}
                Err(error) if error.code == ErrorCode::Conflict => continue,
                Err(error) => return json_response(500, json!({"error": fault_message(&error)})),
            }
            sync_index(&call.host, &run).await;
            return json_response(200, json!({"run": run.view()}));
        }
        // 在飞调用占有该挑战；状态过期由 settle_stale 按已消耗尝试恢复。
        if run.queries.iter().any(|query| query.status == "running") {
            return json_response(
                409,
                json!({"error": "step already in flight", "run": run.view()}),
            );
        }
        // 找到下一道未接受的挑战；全部接受表示可进入本地归因。
        let pending_index = run
            .queries
            .iter()
            .position(|query| query.status != "accepted");
        let Some(position) = pending_index else {
            run.status = "collecting".to_owned();
            run.updated_at_ms = runs::now_ms();
            match runs::save_run(&call.host, &run, Some(version)).await {
                Ok(_) => {}
                Err(error) if error.code == ErrorCode::Conflict => continue,
                Err(error) => return json_response(500, json!({"error": fault_message(&error)})),
            }
            sync_index(&call.host, &run).await;
            return json_response(200, json!({"run": run.view()}));
        };
        let attempts = run.queries[position].attempts.len() as u32;
        if attempts >= runs::MAX_ATTEMPTS {
            let note = run.queries[position]
                .attempts
                .last()
                .and_then(|attempt| attempt.error.clone());
            finish_run(
                &mut run,
                "failed",
                Some(format!(
                    "挑战尝试耗尽（{} 次）：{}",
                    runs::MAX_ATTEMPTS,
                    note.unwrap_or_else(|| "回答未达到要求".to_owned())
                )),
            );
            match runs::save_run(&call.host, &run, Some(version)).await {
                Ok(_) => {}
                Err(error) if error.code == ErrorCode::Conflict => continue,
                Err(error) => return json_response(500, json!({"error": fault_message(&error)})),
            }
            sync_index(&call.host, &run).await;
            return json_response(
                200,
                json!({"run": run.view(), "error": "maximum attempts exceeded"}),
            );
        }
        run.queries[position].status = "running".to_owned();
        // 发送前记账：即使父调用中断，尝试仍占预算，不能无限重发。
        run.queries[position]
            .attempts
            .push(running_attempt(attempts + 1));
        run.status = "running".to_owned();
        run.updated_at_ms = runs::now_ms();
        if let Err(error) = runs::save_run(&call.host, &run, Some(version)).await {
            if error.code == ErrorCode::Conflict {
                continue;
            }
            return json_response(500, json!({"error": fault_message(&error)}));
        }

        return execute_step(call, run, position).await;
    }
    json_response(409, json!({"error": "run is busy, retry"}))
}

fn running_attempt(index: u32) -> AttemptRecord {
    AttemptRecord {
        index,
        status: "running".to_owned(),
        parsed_numbers: None,
        minimum_numbers: None,
        finish_reason: None,
        upstream_model: None,
        input_tokens: None,
        output_tokens: None,
        error: None,
        text_preview: None,
    }
}

async fn execute_step(call: &ManagementCall, run: RunState, position: usize) -> ManagementResult {
    let attempt_index = run.queries[position].attempts.len() as u32;
    let prompt = &run.queries[position].prompt;
    // 模型调用只执行一次；后续 CAS 重试仅重放结果落盘。
    let outcome = model::generate(
        &call.host,
        &run.client_key_id,
        &run.model,
        run.provider.as_deref(),
        run.account_id.as_deref(),
        run.reasoning_effort.as_deref(),
        prompt,
    )
    .await;

    // 原始输出只返回给本次步骤调用方（即发起测试的页面），用于本地解析；
    // 持久化视图只保留数字序列与短预览。
    let outcome_text = outcome.as_ref().map(|outcome| outcome.text.clone()).ok();

    for _ in 0..MAX_CAS_RETRIES {
        // 只结算本次已登记的尝试；取消、过期恢复、删除不能被迟到结果复活。
        let Some((mut current, version)) = (match runs::load_run(&call.host, &run.id).await {
            Ok(found) => found,
            Err(error) => return json_response(500, json!({"error": fault_message(&error)})),
        }) else {
            return json_response(500, json!({"error": "run state disappeared"}));
        };
        let Some(query) = current.queries.get_mut(position) else {
            return json_response(500, json!({"error": "query state missing"}));
        };
        if query.attempts.len() as u32 != attempt_index
            || query
                .attempts
                .last()
                .is_none_or(|attempt| attempt.status != "running")
        {
            return json_response(200, json!({"run": current.view()}));
        }
        query.attempts.pop();
        match outcome.clone() {
            Ok(outcome) => {
                let numbers = parse::parse_numbers(&outcome.text);
                let minimum = (f64::from(query.expected_count) * 0.55).ceil() as usize;
                let minimum = minimum.max(80);
                let accepted = numbers.len() >= minimum;
                query.attempts.push(AttemptRecord {
                    index: attempt_index,
                    status: if accepted { "accepted" } else { "rejected" }.to_owned(),
                    parsed_numbers: Some(numbers.len()),
                    minimum_numbers: Some(minimum),
                    finish_reason: outcome.finish_reason.clone(),
                    upstream_model: outcome.model.clone(),
                    input_tokens: outcome.input_tokens,
                    output_tokens: outcome.output_tokens,
                    error: None,
                    text_preview: Some(
                        outcome
                            .text
                            .chars()
                            .take(TEXT_PREVIEW_CHARS)
                            .collect::<String>(),
                    ),
                });
                if accepted {
                    query.status = "accepted".to_owned();
                    query.numbers = Some(numbers);
                    let mut response = outcome;
                    // 模型输出可能含多字节字符，按合法 UTF-8 边界截断。
                    let end = response.text.floor_char_boundary(RESPONSE_TEXT_BYTES);
                    response.text.truncate(end);
                    query.response = Some(response);
                } else {
                    query.status = "pending".to_owned();
                }
            }
            Err(error) => {
                let permanent = is_permanent(&error);
                query.attempts.push(AttemptRecord {
                    index: attempt_index,
                    status: "failed".to_owned(),
                    parsed_numbers: None,
                    minimum_numbers: None,
                    finish_reason: None,
                    upstream_model: None,
                    input_tokens: None,
                    output_tokens: None,
                    error: Some(fault_message(&error)),
                    text_preview: None,
                });
                // 永久错误（额度/权限/协议不支持）与尝试耗尽都不再重试。
                if permanent || attempt_index >= runs::MAX_ATTEMPTS {
                    query.status = "failed".to_owned();
                } else {
                    query.status = "pending".to_owned();
                }
            }
        }
        if is_terminal(&current.status) {
            // 终态只允许补记真实尝试结果，不重新进入 collecting/running。
        } else if current.cancel_requested {
            finish_run(&mut current, "cancelled", Some("检测已取消".to_owned()));
        } else if current.queries.iter().any(|query| query.status == "failed") {
            let note = current
                .queries
                .iter()
                .filter(|query| query.status == "failed")
                .flat_map(|query| query.attempts.iter())
                .last()
                .and_then(|attempt| attempt.error.clone());
            finish_run(
                &mut current,
                "failed",
                Some(note.unwrap_or_else(|| "挑战尝试耗尽".to_owned())),
            );
        } else if current
            .queries
            .iter()
            .all(|query| query.status == "accepted")
        {
            current.status = "collecting".to_owned();
            current.status_note = None;
        } else if current.status != "running" {
            // 从 interrupted 恢复后保持 running；其余状态维持不变。
            current.status = "running".to_owned();
        }
        current.updated_at_ms = runs::now_ms();
        match runs::save_run(&call.host, &current, Some(version)).await {
            Ok(_) => {
                sync_index(&call.host, &current).await;
                return json_response(
                    200,
                    json!({"run": current.view(), "response_text": outcome_text}),
                );
            }
            Err(error) if error.code == ErrorCode::Conflict => continue,
            Err(error) => return json_response(500, json!({"error": fault_message(&error)})),
        }
    }
    json_response(409, json!({"error": "run is busy, retry"}))
}

async fn cancel_run(call: &ManagementCall) -> ManagementResult {
    let request: RunRef = match decode_body(call) {
        Ok(request) => request,
        Err(error) => return json_response(400, json!({"error": error.message})),
    };
    let Some((mut run, version)) = (match runs::load_run(&call.host, &request.id).await {
        Ok(found) => found,
        Err(error) => return json_response(500, json!({"error": fault_message(&error)})),
    }) else {
        return json_response(404, json!({"error": "run not found"}));
    };
    if matches!(run.status.as_str(), "completed" | "cancelled" | "failed") {
        return json_response(200, json!({"run": run.view()}));
    }
    let mut version = version;
    for _ in 0..MAX_CAS_RETRIES {
        if is_terminal(&run.status) {
            return json_response(200, json!({"run": run.view()}));
        }
        run.cancel_requested = true;
        let in_flight = run.queries.iter().any(|query| query.status == "running");
        finish_run(
            &mut run,
            "cancelled",
            Some(if in_flight {
                "检测已取消，在飞步骤仍可能产生消耗并补记结果".to_owned()
            } else {
                "检测已取消".to_owned()
            }),
        );
        match runs::save_run(&call.host, &run, Some(version)).await {
            Ok(_) => {
                sync_index(&call.host, &run).await;
                return json_response(200, json!({"run": run.view()}));
            }
            Err(error) if error.code == ErrorCode::Conflict => {
                match runs::load_run(&call.host, &request.id).await {
                    Ok(Some((fresh, fresh_version))) => {
                        run = fresh;
                        version = fresh_version;
                    }
                    Ok(None) => return json_response(404, json!({"error": "run not found"})),
                    Err(error) => {
                        return json_response(500, json!({"error": fault_message(&error)}));
                    }
                }
            }
            Err(error) => return json_response(500, json!({"error": fault_message(&error)})),
        }
    }
    json_response(409, json!({"error": "cancel write conflicted, retry"}))
}

async fn report_run(call: &ManagementCall) -> ManagementResult {
    let request: RunRef = match decode_body(call) {
        Ok(request) => request,
        Err(error) => return json_response(400, json!({"error": error.message})),
    };
    let Some(result) = request.result else {
        return json_response(400, json!({"error": "missing result"}));
    };
    let Some((mut run, version)) = (match runs::load_run(&call.host, &request.id).await {
        Ok(found) => found,
        Err(error) => return json_response(500, json!({"error": fault_message(&error)})),
    }) else {
        return json_response(404, json!({"error": "run not found"}));
    };
    if run.status != "collecting"
        || run.cancel_requested
        || !run.queries.iter().all(|query| query.status == "accepted")
    {
        return json_response(
            409,
            json!({"error": "run is not ready for attribution", "run": run.view()}),
        );
    }
    run.result = Some(result);
    run.status = "completed".to_owned();
    run.completed_at_ms = Some(runs::now_ms());
    run.updated_at_ms = runs::now_ms();
    if let Err(error) = runs::save_run(&call.host, &run, Some(version)).await {
        return json_response(500, json!({"error": fault_message(&error)}));
    }
    runs::touch_index(&call.host, run.index_entry()).await;
    json_response(200, json!({"run": run.view()}))
}

async fn delete_run(call: &ManagementCall) -> ManagementResult {
    let request: RunRef = match decode_body(call) {
        Ok(request) => request,
        Err(error) => return json_response(400, json!({"error": error.message})),
    };
    let Some((_run, version)) = (match runs::load_run(&call.host, &request.id).await {
        Ok(found) => found,
        Err(error) => return json_response(500, json!({"error": fault_message(&error)})),
    }) else {
        // 正文已不存在但索引可能还有遗留条目：顺手清理再报告 404。
        let _ = runs::remove_index(&call.host, &request.id).await;
        return json_response(404, json!({"error": "run not found"}));
    };
    let params = match serde_json::to_value(StateDeleteRequest {
        namespace: runs::NAMESPACE.to_owned(),
        key: format!("run-{}", request.id),
        expected_version: version,
    }) {
        Ok(params) => params,
        Err(_) => {
            return json_response(500, json!({"error": "state payload encode failed"}));
        }
    };
    match call
        .host
        .call("host.state.delete", params, Vec::new())
        .await
    {
        Ok(reply) => {
            let _ = serde_json::from_value::<StateDeleteResult>(reply.result);
            if let Err(error) = runs::remove_index(&call.host, &request.id).await {
                return json_response(500, json!({"error": fault_message(&error)}));
            }
            json_response(200, json!({"ok": true}))
        }
        Err(error) => json_response(500, json!({"error": error.into_plugin_fault().message})),
    }
}

fn registration() -> ManagementRegistration {
    fn route(method: &str, path: &str, request_types: &[&str]) -> ManagementRoute {
        ManagementRoute {
            method: method.to_owned(),
            path: path.to_owned(),
            request_content_types: request_types.iter().map(|item| item.to_string()).collect(),
            response_content_types: vec!["application/json".to_owned()],
        }
    }
    ManagementRegistration {
        routes: vec![
            route("GET", "bootstrap", &[]),
            route("GET", "models", &[]),
            route("GET", "settings", &[]),
            route("POST", "settings", &["application/json"]),
            route("POST", "settings-reset", &["application/json"]),
            route("POST", "runs", &["application/json"]),
            route("GET", "runs", &[]),
            route("POST", "run/step", &["application/json"]),
            route("POST", "run/cancel", &["application/json"]),
            route("GET", "run", &[]),
            route("POST", "run/report", &["application/json"]),
            route("POST", "run/delete", &["application/json"]),
        ],
        resources: vec![
            ManagementResource {
                path: "ui/index.html".to_owned(),
                public: false,
            },
            ManagementResource {
                path: "ui/app.js".to_owned(),
                public: false,
            },
            ManagementResource {
                path: "ui/trace.js".to_owned(),
                public: false,
            },
            ManagementResource {
                path: "ui/style.css".to_owned(),
                public: false,
            },
            ManagementResource {
                path: "ui/icon.svg".to_owned(),
                public: false,
            },
            ManagementResource {
                path: "ui/data/unified_bank.js".to_owned(),
                public: false,
            },
        ],
        pages: vec![ManagementPage {
            id: "model-trace".to_owned(),
            title: "模型指纹检测".to_owned(),
            description: Some(
                "选择账号与模型，发送三条长整数挑战并按 ModelTrace 指纹库归因".to_owned(),
            ),
            entry: "ui/index.html".to_owned(),
            icon: Some("ui/icon.svg".to_owned()),
        }],
        callbacks: vec![],
    }
}

#[cfg(test)]
mod tests {
    use gateway_plugin_sdk::{Capability, Manifest};

    #[test]
    fn manifest_parses_and_declares_management() {
        let manifest = Manifest::from_author_slice(include_bytes!("../plugin.json")).unwrap();
        assert_eq!(manifest.manifest_version, 2);
        assert!(manifest.contributes.contains_key(&Capability::Management));
    }
}
