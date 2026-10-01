# ModelTrace 模型指纹检测插件

codex-proxy-rs 的管理端插件：选择执行 Key、上游账号与模型，向目标模型发送
1–3 条长整数挑战，解析回答中的数字序列，并在页面本地按
[ModelTrace](https://xqy2006.github.io/ModelTrace/) 统一指纹库做模型归因。

## 能力

- 指纹挑战：每题要求输出 100–400 个 1–355 的整数，最多 3 题，每题最多 4 次尝试（含首次调用与中断）。
- 执行身份：可选 client Key + 指定上游账号（或自动调度），调用走宿主
  `host.model.execute`，插件不接触任何凭据。
- 本地归因：Hellinger + 有序块联合特征，页面内完成，不外发回答。
- 进度：挑战进度条按已接受题目计数，显示当前挑战、第几次调用与累计耗时；不伪造模型生成的实时百分比。轮询失败或上一步结果未确认时有可见提示，可点「继续检测」恢复中断任务。
- 历史记录：运行状态持久化在插件 state 命名空间，支持详情、继续、删除，可按关键词与状态筛选。每次调用发送前占用挑战并计入最多 4 次的尝试预算，结果保存冲突不会重新调用模型；超过 5 分钟未更新的步骤标记为中断后可恢复。取消须保存成功才生效，在飞调用仍可能产生消耗并补记结果，不保证立即停止上游。
- 设置：挑战数量（1–3）、历史展示条数（4–64）与推理强度
  （自动/low–max，按目录 `supported_reasoning_levels` 裁剪）可持久化。

## 目录

| 路径 | 说明 |
| --- | --- |
| `plugin.json` | 插件清单（管理页 + `trace` 状态命名空间） |
| `src/` | 插件进程：管理路由、模型回调封装、运行状态机 |
| `ui/` | 管理页面：表单、进度、归因结果、历史与请求详情 |
| `ui/data/unified_bank.js` | ModelTrace 统一指纹库（`window.MODEL_TRACE_BANK`） |

## 构建与打包

工具链固定 `rust-toolchain.toml`（1.97.0）。SDK 通过 git rev 固定：

```bash
cargo build --release --target <triple>
# 在 codex-proxy-rs 仓库构建 cpr-plugin 后：
cpr-plugin package --manifest plugin.json \
  --binary target/<triple>/release/model-trace \
  --target <triple> --output-dir dist
```

产物 `dist/xunzhimeng.model-trace-<version>-<triple>.tar.gz` 通过管理端
「插件 → 上传」安装。
