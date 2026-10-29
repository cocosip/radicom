# 网络服务层评审（`radicom-network` 服务/引擎层）

评审对象：`crates/radicom-network/src/` 下服务与引擎层文件 —— `server.rs`、`client.rs`、`engine.rs`、`services.rs`、`cfind.rs`、`cget.rs`、`cmove.rs`、`store.rs`、`assembler.rs`、`expert.rs`、`hosting.rs`、`observability.rs`。

不在本文范围（由协议层评审负责）：`pdu.rs`、`transport.rs`、`tls.rs`、`association.rs`、`dimse.rs`、`message.rs`、`extended.rs`、`nservice.rs`、`print_sop.rs`。

参照实现：fo-dicom `FO-DICOM.Core/Network/` 的 `DicomServer.cs`、`DicomService.cs`、`DicomServerRegistry.cs`、`Client/DicomClient.cs`、`Client/DicomClientOptions.cs`、`DicomResponse.cs`、`DicomRequest.cs`、`IDicom*Provider.cs`、`Log/Metrics/INetworkMetricsCollector.cs`，以及 DICOM PS3.7（DIMSE）、PS3.4（服务类，C-ECHO/C-STORE/C-FIND/C-GET/C-MOVE 与 DIMSE-N）、PS3.8（Upper Layer Protocol）。

方法：逐函数读源码，顺着真实调用链（连接 → 协商 → 命令分发 → provider → 响应）走通后再下判断；只在读完代码并确认调用点可达后判定"缺失"。所有性能数据为本轮实测（release 构建，22 核机器，探针与基准放在系统临时目录，未进入仓库）。repo 内 `docs/design/05-network.md`、`docs/notes/01-fo-dicom-alignment-audit.md` 的自评不作为实现证据。本文同时记录已完成修复及其回归证据；实现状态以当前源码和测试为准。

### 证据强度

| 标注 | 含义 |
| --- | --- |
| **实测** | 本轮写了探针程序／基准，真实跑出了该行为（含 socket 级复现与吞吐/并发数字）。强证据。 |
| **源码比对** | 逐行比对 radicom 与 fo-dicom 两侧，或顺着调用链推导，未构造运行时复现。 |
| **grep 确认** | "缺失"类结论先用 grep 确认全仓无对应符号，再标注。 |

**基线**：最近一次验证为 `radicom-network` 330 个单元测试通过，fuzz/golden/自互操作测试通过，workspace rustdoc 与 clippy 均无告警；需要真实 `RADICOM_PEER` 的外部对端测试仍显式 ignored。

**总体结论**：单连接、单请求的服务骨架（C-ECHO / C-STORE / C-FIND / C-GET / C-MOVE + 六个 DIMSE-N 方法）是完整可用的；C-FIND/C-MOVE/C-GET 已逐条消费 provider stream，三种查询/检索服务均能中断在途 provider，`max_clients`、called-AE 过滤与服务端 ACSE 超时也已接线。当前主要服务层缺口是公开 client 的并发取消 API、请求队列语义，以及默认 C-STORE sink 的落盘性能。协议错误、临时文件生命周期、跨 SOP 类重协商和 retrieve 计数已有回归覆盖。性能上服务端并发仍受 `radicom` 解析器全局读锁限制，服务层是暴露点而非病灶。

本次评审共发现：

| 级别 | 数量 | 证据强度 | 说明 |
| --- | --- | --- | --- |
| 功能缺失/部分实现 | 21 行（审计时缺失 12 ／ 部分实现 9；现均修复或定性为有意） | grep 确认 + 源码比对 | 见 §1 |
| 高（正确性） | 5 | 实测 §2.1–§2.2、§2.6；源码比对 §2.3–§2.5 | 可触发崩溃、数据丢失、静默错误 |
| 中（正确性） | 7 | §2.7–§2.13，源码比对为主 | 规范偏差、边界语义错误 |
| 低（正确性） | 8 | §2.14–§2.20，源码比对 | 健壮性、死代码、文档与行为不符 |
| 性能 | 9 | 实测 + 源码比对 | 见 §3 |
| 测试问题 | 10 | 实测/读测试代码 | 假绿、同义反复、覆盖空洞 |

---

## 1. 功能缺失（对比 fo-dicom 服务层）

状态取值：**已实现** / **部分实现** / **缺失** / **有意不实现**。"缺失"均已 grep 确认全仓无对应实现。

> 判定口径：**以功能是否达成为准**，不要求与 fo-dicom 同名的 .NET 类/接口/结构一一对应。Rust 按其生态
> 惯例实现（tokio/`trait` 而非 `IDicom*Provider` 等 .NET 接口的形状照搬）；功能等价即视为已实现，
> .NET 专有抽象在 Rust 无必要者标“不适用/有意不实现”。

### 1.1 服务端（SCP）

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| C-ECHO SCP | `DicomCEchoProvider` | 已实现 | `server.rs:333-337`、`services.rs:272-287` `VerificationProvider` | — |
| C-STORE SCP | `DicomCStoreProvider` | 已实现 | `server.rs:672-702` | 默认 sink 生命周期已修复，落盘性能问题见 §3.1 |
| C-FIND SCP | `DicomCFindProvider` | 已实现 | `server.rs:757-850` | Pending 结果逐条发送；同一 PC+message ID 的 C-CANCEL 会中断在途 provider（丢弃其流）。 .NET 的 provider 取消回调是接口细节，Rust 用 future 取消等价实现 |
| C-GET SCP | `DicomCGetProvider` | 已实现 | `server.rs:567-707` | provider 事件逐条消费；Pending 的 `remaining` 由 provider 提供；C-CANCEL 可中断在途 provider |
| C-MOVE SCP | `DicomCMoveProvider` | 已实现 | `server.rs:669-808` | Pending/终止响应逐条发送；C-CANCEL 可中断在途 provider |
| C-CANCEL 处理 | `DicomService.cs` 取消在途请求 | 已实现 | `server.rs:338-347`、`server.rs:590-895` | 流式 C-FIND/C-MOVE/C-GET 在途时回送 `CANCEL` 终止响应并取消 provider future；功能达标（无 .NET 式 provider 回调，属结构差异） |
| N-EVENT-REPORT SCP | `IDicomNEventReportRequestProvider` | 已实现 | `services.rs:240-243`、`server.rs:404` | `server.rs` 有 N-service 回环覆盖 |
| N-GET / N-SET SCP | `IDicomNServiceProvider` | 已实现 | `services.rs:245-258`、`server.rs:405-410` | `server.rs` 有 N-service 回环覆盖 |
| N-ACTION / N-CREATE SCP | `IDicomNServiceProvider` | 已实现 | `services.rs:255-263`、`server.rs:411-416` | N-ACTION/N-CREATE 均有回环覆盖 |
| N-DELETE SCP | `IDicomNServiceProvider` | 已实现 | `server.rs:419-421` | 有测试（`server.rs:2883`） |
| 可插拔 provider | `IDicomC*Provider` 全套接口（.NET 每服务一个接口） | 已实现（功能等价） | `services.rs` `ServiceProvider`（单一 trait，默认方法为“不支持”） | 用**一个 trait + 默认方法**达到"按服务插拔"：实现者只覆盖需要的服务。 .NET 拆成多个接口是其接口体系习惯，Rust 单 trait 更简洁且功能等价 |
| 服务注册表 `(port, ip)` | `DicomServerRegistry` / `IDicomServerRegistry`（`DicomServerRegistry.cs:12-40`） | **已实现** | `registry.rs` `DicomServerRegistry`；`server.rs:146` 绑定时登记、释放时自动注销 | 进程内可查询地址占用；操作系统仍负责最终 bind 校验 |
| `max_clients` 限流 | `DicomServer.cs:185-188` `SemaphoreSlim(MaxClientsAllowed)`、`:279-289` 等待配额 | **已实现** | `server.rs:179-210` 使用可选 `Semaphore` 门控 accept loop；`max_clients=0` 保持不限流；`max_clients_bounds_concurrent_associations` 覆盖满槽位与释放 |
| 已连接客户端数 | `DicomServer.cs:201-204` `GetNumberOfConnectedClients` | **已实现** | `hosting.rs:188-194`、`server.rs:166-168` | `ConnectedClients` 原子计数跟踪 live connection task；有启动/释放回归测试 |
| 服务端 A-ASSOCIATE-RQ 读超时 | `DicomService.cs` ACSE 阶段超时 | **已实现** | `server.rs:269-281` 用 `ServerOptions.association_timeout` 包裹首个 PDU 读取，超时发送 Provider A-ABORT；`an_association_that_never_sends_a_request_times_out` 覆盖 |
| AE title 过滤 | `DicomServer.cs` called-AE 匹配 | **已实现** | `hosting.rs:58-62`、`server.rs:282-290` | hosted 配置把 AE title 传给 `ServerOptions`，A-ASSOCIATE-RQ 不匹配时返回 Permanent Reject |
| 协议违规时发 A-ABORT | `DicomService.cs:1716-1717` `SendAbortAsync` | **已实现** | `server.rs:235-253` `serve_connection` 在协议错误返回前发送 Provider A-ABORT；响应型 command 保留 User-source A-ABORT | 未知 PDU 等协议错误会先收到 reason 0 的 Provider A-ABORT；回归测试覆盖 |
| 指标钩子 | `INetworkMetricsCollector`（`DataSent/DataReceived/ConnectionEstablished/ConnectionClosed`） | 已实现（超集） | `observability.rs:26-33` | radicom 多了 association 开合两个钩子，属于超集，不是缺陷 |

### 1.2 客户端（SCU）

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| C-ECHO / C-STORE / C-FIND / C-GET / C-MOVE SCU | `DicomClient` + `DicomC*Request` | 已实现 | `client.rs:211/227/248/279/261` | 每个方法自带 `PresentationContext::new(1, ...)`，PC id 恒为 1 |
| 请求队列 + 一次协商全部 PC | `DicomClient.AddRequestAsync` + `SendRequestAsync`（.NET 异步队列） | 已实现（Rust 表达） | `client.rs:172-184` `ensure_association` | 同步 API 无异步队列；`ensure_association` 检查当前关联是否覆盖本次 SOP class，不足时释放并重开，功能上满足多 SOP 请求 |
| 每请求响应最多 N 次超时监控 | `DicomService.cs:1516` `WarningRequestTimedOut` + `CheckForTimeouts` | **已实现** | `server.rs` `ClientOptions::request_timeout`；`Association` 所有 DIMSE 响应等待均有超时 | 超时返回 `NetworkError::RequestTimedOut`，高层 client 发出事件并中止关联 |
| 请求级 watchdog | `_pending` + `Options.RequestTimeout`（.NET 异步队列） | ⛔ 不适用 | `ClientOptions::request_timeout` | .NET 的多请求 watchdog 服务于其异步队列；radicom 同步 API 每次等待单个响应并直接施加 `request_timeout`，无需独立扫描 |
| 关联事件 `StateChanged` | `DicomClient.cs:215-221`（6 个事件） | **已实现** | `client.rs:56` `ClientEvent::StateChanged`；接受、释放和中止路径发出 `Open`/`Closing`/`Closed` | 与既有离散事件并存，统一报告关联生命周期状态；回归测试覆盖 |
| `AssociationRequestTimedOut` 事件 | `DicomClient.cs:215-221` | **已实现** | `client.rs` `ClientEvent::AssociationRequestTimedOut`；超时尝试在 `open_association` 发出 | 每次可重试的关联超时都会携带累计尝试次数 |
| 取消模式 `DicomClientCancellationMode` | `DicomClient.cs:175` `{ImmediatelyReleaseAssociation, ImmediatelyAbortAssociation}` | **已实现** | `client.rs` `DicomClientCancellationMode`；`DicomClient::shutdown(mode)` | 默认 `ImmediatelyReleaseAssociation`；`shutdown` 按模式释放或中止关联，覆盖与既有 `release`/`abort` 同一路径 |
| 监听式 linger（队列空等 N ms） | `AssociationLingerTimeoutInMs` 默认 50 | ⛔ 不适用 | `client.rs` 配额用尽时 sleep | 同步 API 无请求队列，故无"队列空等"语义；仅在配额用尽时短暂 sleep 后重开关联 |
| 每关联请求配额 | `MaximumNumberOfRequestsPerAssociation` | 已实现 | `client.rs:30`、`client.rs:129-131` | 语义一致 |
| 关联请求重试上限 | `MaximumNumberOfConsecutiveTimedOutAssociationRequests`（默认 3） | **已实现** | `client.rs:187-229` | 仅超时/可重试 IO 重试；0 表示不重试，拒绝/TLS/协议错误立即返回；见 §2.8 |
| C-CANCEL SCU | `DicomCCancelRequest` | ⛔ 设计边界（同步 API） | `client.rs:314-320` | 记录最近一次请求的 presentation context；同步阻塞的 `send_c_find` 期间无法从同一 client 并发 `cancel`，需上层用独立任务/连接取消 |
| 反向 C-STORE 处理器（C-GET 收图） | `IDicomCStoreProvider` on SCU | 已实现 | `server.rs:1005-1007`、`server.rs:1161-1171` | — |
| Expert/Advanced 层 | `Client/Advanced/*` | 已实现 | `expert.rs` `Connection`、`send` / `send_streaming` | `send_streaming` 复用关联的 `*_stream` 逐条回调、不整体缓冲；`send` 保留为返回缓冲迭代器的便捷入口。已删除未使用的 `RELEASE_TIMEOUT`（释放超时由关联的 `release_timeout` 负责） |

### 1.3 明确"有意不实现" vs 真实缺口

以下三处是**代码里写明的取舍**，不算缺陷，但把上限记在这里：

| 项 | 位置 | 声明 | 判断 |
| --- | --- | --- | --- |
| role selection 默认不协商 | `association.rs:263-275` `accept_roles: false` | 默认拒绝 SCU/SCP 角色选择 | 有意取舍；但意味着 SCP 不能作为 SCU 反向发请求，C-GET 只能走同关联反向 C-STORE |
| `expert.rs` 的 `RELEASE_TIMEOUT` | — | 原常量声明 10s | **已修复**：该常量已删除；释放超时由 `Association::release` 的 `release_timeout`（默认 10s）统一负责 |

仍属于真实缺口且无代码层取舍说明的是：

- **公开 client 的并发 C-CANCEL 仍受 API 借用模型限制**：底层 `Association` 已支持三种服务的取消；`DicomClient::send_*` 持有 `&mut self` 直到终止响应，无法同时调用 `cancel`。

---

## 2. 正确性缺陷

### 2.1 【已修复】对端可触发 panic：任意响应型 command field 到达即 `.expect()` 崩溃

该问题已修复：服务端现在把协议错误转换为 Provider A-ABORT 后关闭关联；响应型 command 仍走既有的 User-source A-ABORT 路径。`a_response_command_aborts_the_association_instead_of_panicking` 和 `protocol_errors_after_association_send_a_provider_abort` 覆盖这两条行为。以下保留原始复现证据，便于追溯问题背景。

位置：`dimse.rs:225`（`Command::response` 里 `response_field(...).expect(...)`），触发点 `server.rs:354-359`。

```rust
// dimse.rs:224-225
pub fn response(request: &Command, status: u16) -> Self {
    let field = response_field(request.field).expect("request field has a response counterpart");
```

`handle_command` 的兜底分支对**任何**未识别 field 都调 `serving.fail(...)`：

```rust
// server.rs:354-359
other => {
    // unsupported request: answer ProcessingFailure, keep the connection
    serving
        .fail(context_id, &command, &format!("unsupported command {other:?}"))
        .await
}
```

而 `serving.fail`（`server.rs:752-756`）正是 `Command::response`。`response_field`（`dimse.rs:347-362`）对 11 种响应 field（`0x8010..0x80C0`）和 C-CANCEL 一律返回 `None`。于是**对端只要在已建立的关联里发一条响应型 command 或 C-CANCEL 之外的未知 field，服务端 task 就 panic**。

实测（真实 TCP，探针连上服务端并按正常 P-DATA 分帧发送命令集）。触发条件是**已建立关联之后**，任意一个 `(0000,0100) CommandField` 落在响应区 `0x8010..0x80C0`（或 C-CANCEL 之外的任何未识别 field）的 command set。以下 52 字节是经实测确认的最小可用触发载荷（`command_bytes(&Command::request(CommandField::CFindRsp, 1))` 的输出，一个 P-DATA-TF 打包，共 64 字节上线）：

```
00 00 00 00 04 00 00 00 28 00 00 00   ; (0000,0000) UL 4 = 0x28 (group length)
00 00 00 01 02 00 00 00 20 80         ; (0000,0100) US = 0x8020  ← C-FIND-RSP
00 00 10 01 02 00 00 00 01 00         ; (0000,0110) US = 1 (Message ID)
00 00 00 07 02 00 00 00 01 00         ; (0000,0700) US = 1 (Priority)
00 00 00 08 02 00 00 00 01 00         ; (0000,0800) US = 1 (no dataset)
```

服务端输出：

```
thread 'tokio-rt-worker' panicked at crates\radicom-network\src\dimse.rs:225:51:
request field has a response counterpart
server reply after C-FIND-RSP as request: Ok(None)      ← 对端只看到 FIN，没有 A-ABORT
server task: Err(JoinError::Panic(Id(23), "request field has a response counterpart", ...))
```

同样的 52 字节把 field 换成 `0x8030`（C-ECHO-RSP）也复现，换成带 `(0000,0002)` SOP class 的 88 字节版本同样复现——即**一整类** field 都触发，不是某一个特例。

**爆炸半径（重要，必须准确）**：默认配置下是**连接级**，不是进程级。实测同一次运行里 accept loop 存活、新客户端仍能连上：

```
victim socket reply: Ok(false)                    ← 这条连接静默被丢，无 A-ABORT
accept loop alive (new client connects): true
server loop task still running: true
```

原因是 panic 发生在 `tokio::spawn` 出的连接 task 里（`server.rs:135`），tokio 捕获后转成 `JoinError::Panic`，而 accept loop 是另一个 task。但是有两个前提必须写清：其一，若部署时打开 `panic = "abort"`（服务端二进制常见选择），同一段代码立刻变成**进程级 remote DoS**——一个 64 字节的 P-DATA-TF 打死整个服务；其二，当前 workspace 所有 `Cargo.toml` 均未设置 `panic`，即默认 unwind，所以**现在**是连接级。另外即便只丢连接，也是静默丢失：该连接上所有在途请求的调用方都收不到任何状态，且对端收不到 A-ABORT，运维侧看到的是"对端莫名断开"而非协议错误。

正确行为：`response_field` 返回 `None` 时回 A-ABORT（source=service-user, reason=not-specified）或直接丢弃该 PDU，绝不 `expect`。

可达性已逐一核对：`fail`（`server.rs:752`）共 11 个调用点，但多数被前置 field 判断挡住（如 `server.rs:683` 的 `command.field != CStoreRq`、`server.rs:411` 的 `is_n_request` 守卫）。真正能被对端直达的是 `server.rs:354-359` 的兜底 `other =>` 分支——落到那里的 field 已经是"未被任何服务识别"，正是响应型 field 与 C-CANCEL 的形状。

### 2.2 【已修复】默认 C-STORE sink 临时文件生命周期

旧实现成功解析后丢弃临时路径，导致 `%TEMP%\radicom-store-*.dcm` 无界增长。当前 `store.rs:68-80` 用 `ReceivedInstance::temp: TempSpool` 持有路径，并在 `Drop` 中删除；解析失败路径也继续清理。`spool_to_temp_removes_its_file_with_the_instance` 现在检查实际路径存在，并断言实例释放后文件消失。

`SpoolToTemp` 仍会产生一次落盘和写入开销，性能问题保留在 §3.1；需要持久化的调用方应使用 `SpoolToDir`，只需内存解析时使用 `Memory`。

### 2.3 【已修复】C-MOVE 兜底终端响应曾丢弃真实子操作计数

此前位置：`server.rs:594-599`；当前实现位于 `server.rs:727-757`。

```rust
if !terminated {
    // a provider that never terminated its stream still gets a final answer
    let response = crate::cmove::CMoveResponse::completed(&request, summary);
    serving.send(context_id, &response.to_command()).await?;
}
```

`store_to_destination`（`cmove.rs:284-320`）返回真实 `SubOperationSummary`。旧实现会在 provider 只发 Pending 不发终端时用**全 0 计数**收尾；当前实现保留循环中最后一个 Pending 的计数，兜底终端响应带出真实结果。PS3.4 C-MOVE 的终端响应必须带 `(0000,1021/1022/1023)` 实际计数，`cmove.rs:256` 也确实把它们写进命令集。

回归测试 `c_move_reports_the_real_counts_when_the_provider_never_terminates` 覆盖了该兜底路径；C-MOVE 现在还会在 provider 产生终止响应前逐条发送 Pending。

### 2.4 【已调整】C-GET 的 `remaining` 由 provider 进度驱动

此前位置：`server.rs:479`、`server.rs:492-494`；当前实现位于 `server.rs:567-665`。

```rust
while let Some(event) = stream.next().await {
    // Pending CGetResponse values are forwarded as they arrive.
}
```

旧实现的问题有三层：

1. **不是真实剩余量**。provider 完全可以只发 `Response` 而把实例通过其它渠道交付（或先发 Response 后发 Instance）；此时 `remaining=0` 却仍有实例要送。
2. **不是增量进度**。`(0000,1020)` 在 PS3.4 C-GET 中定义为"还需完成的子操作数"，随每个子操作递减。这里只能对预缓冲的快照递减，provider 若中途再产出实例则计数失效。
3. **计数与 SCU 观察不一致**。SCU 侧看到的是"剩 N"→"剩 N-1"…但 N 来自缓冲长度，与 terminal 里的 completed 无必然关系。

当前行为：由 provider 的 `pending(remaining, summary)` 驱动 `(0000,1020)`，服务端逐条转发；这样无需把所有实例收进内存。实例已经交给反向 C-STORE 后，终止计数以实际子操作结果为准，避免 provider 的旧计数掩盖传输失败。若 provider 不产生 Pending 响应，服务端无法从流式实例序列推导总剩余数，因此只发送终止响应；provider 应在需要进度时显式产生 Pending。`c_get_reports_the_remaining_sub_operations` 覆盖了 provider 计数转发。

### 2.5 【已修复】服务端 A-ASSOCIATE-RQ 读超时

旧实现会让静默连接永久占用 task 和 fd。当前 `server.rs:269-281` 在 ACSE 首个 `read_pdu` 外包裹 `ServerOptions.association_timeout`，超时后发送 Provider A-ABORT 并返回 `AssociationRequestTimedOut`。`an_association_that_never_sends_a_request_times_out` 覆盖静默 TCP 连接、A-ABORT source/reason 和错误类型。

### 2.6 【已修复】`DicomClient` 会为缺少的 SOP context 重建关联

`ensure_association` 现在用 `covers` 检查当前协商结果；已有关联缺少本次请求的抽象语法时先释放，再用新的 presentation context 集合重开。`a_second_sop_class_gets_its_own_association` 覆盖 C-ECHO 后 C-STORE 的路径。

### 2.7 【中】`linger` 不实现队列语义，只按配额睡眠

`client.rs:133-143` 的注释诚实说明了这点，但仍要记为偏差：fo-dicom 的 `Linger` 是"发送队列排空后再等 50ms，看有没有新请求"。这里 `retire_if_spent` 只在 `quota_spent()` 为真时 sleep，等于给配额耗尽加了个延迟释放，与"等待更多工作"无关。`client.rs:10` 的状态机注释（`→ SEND (drain) → LINGER`）与实现不符。

### 2.8 【已修复】关联重试只针对可重试传输错误

`open_association` 现在只重试关联超时、`TimedOut` 和 `WouldBlock` IO；拒绝、TLS 错误和协议错误立即返回。`max_consecutive_timeout_retries = 0` 表示不重试，相关回归测试覆盖。

### 2.9 【已修复】消息 ID 回绕会跳过在途请求

实际发送路径使用 `Association` 内的 `engine::PendingTable` 分配 ID。分配器从 `u16::MAX` 回到 1，并跳过仍在途的 ID；`message_ids_wrap_around` 和在途跳过测试覆盖回绕边界。

### 2.10 【已修复】C-GET 将非致命实例错误计为失败并继续

`is_fatal` 只把 IO、Abort 和过早关闭视为连接级错误；缺少 SOP/UID、转码或单实例传输错误会记录 Failed Sub-operation，继续处理后续实例并发送终止响应。

### 2.11 【已修复】PDV assembler 按 context 与 command/data 流隔离

`PdvAssembler` 现在按 `(context_id, Stream)` 保存独立缓冲，交错 presentation context 或 command/data PDV 不会互相拼接；服务端待数据集命令也按 context 排队。assembler 与服务回环测试覆盖交错输入。

### 2.12 【中】`SubOperationSummary::record` 把 Cancel 归入 failed，且 Warning 独立成桶

位置：`cmove.rs:36-42`。

```rust
pub fn record(&mut self, state: StatusState) {
    match state {
        StatusState::Success => self.completed += 1,
        StatusState::Warning => self.warning += 1,
        _ => self.failed += 1,          // Cancel 也算 failed
    }
}
```

`StatusState::Cancel`（`dimse.rs` `classify` 对 `0xFE00`）落进 `failed`。PS3.4 里 Warn 是"完成但需注意"，通常应与 completed 一起上报（fo-dicom `DicomCMoveResponse.Completed`/`Warnings` 是分开的两个 tag，但 Completed 包含带 warning 的子操作）。这里把它们互斥分桶，会让 `final_status()`（`cmove.rs:47-58`）在"全部带 warning"时报 `SUBOPERATIONS_COMPLETED_WITH_WARNINGS` 而不是预期语义；更重要的是 `record(Cancel)` 在 provider 侧**永远不该出现**（Cancelled 不是子操作终态），却静默污染 failed 计数。

### 2.13 【低】`Association::release` 不校验对端响应，也无超时

位置：`server.rs:1695-1700`。

```rust
pub async fn release(mut self) -> Result<()> {
    write_pdu(&mut self.stream, &Pdu::ReleaseRq).await?;
    let _ = read_pdu(&mut self.stream).await;   // 结果丢弃
    Ok(())
}
```

收到 A-ABORT、收到 P-DATA、或对端彻底不响应（永久挂住，`read_pdu` 无 timeout），四种情况都返回 `Ok(())`。fo-dicom 有 `AssociationReleaseTimeoutInMs`（默认 10000）。这也是 §1.2 "无请求超时体系"的一部分。

### 2.14 【低】`store.rs:175` 的测试名与断言不符

见 §4.2，属测试缺陷，但根源在实现把路径丢掉了。

### 2.15 【低】`command_bytes` 文档说 explicit VR，实际编码 implicit VR

位置：`server.rs:856-870`。

```rust
/// PS3.5 §7.7 fixes every command element's VR in a table of its own, so the
/// command set is encoded with explicit VRs: the receiver then never needs a
/// dictionary entry for group 0000 ...
pub fn command_bytes(command: &Command) -> Result<Vec<u8>> {
    ...
    .write_dataset(&dataset, &syntaxes::IMPLICIT_VR_LITTLE_ENDIAN, &mut out)   // ← implicit
```

行为（implicit VR LE）**是正确的**——PS3.7 §9.3 规定命令集恒用 Implicit VR LE。是文档字符串写错，且测试 `server.rs:1734` `command_bytes_are_implicit_vr_little_endian` 断言的也是 implicit。属文档缺陷，但会误导下一个改这里的人。

### 2.16 【低】服务端把对端 A-ABORT 当干净关闭

位置：`server.rs:303`。

```rust
Pdu::Abort(_) => return Ok(()),
```

对端 abort 说明会话异常结束（PS3.8 §9.3.8），但返回 `Ok(())` 会让 `ConnectionGuard`/`AssociationGuard`（`observability.rs:247-270`）按正常关闭上报指标，运维侧无法区分正常释放与异常中断。对比 `server.rs:782-784`（反向 C-STORE 等待路径）正确地把它转成 `NetworkError::Abort`。

### 2.17 【已修复】`HostedDicomServer::stop` 停止接收时保留在途关联

位置：`hosting.rs`、`server.rs::run_until`。

`HostedDicomServer::stop` 现在通过 watch 信号停止 accept loop，等待监听任务退出，但不再 abort 已创建的连接 task，也不清空 `connected_clients`。活动关联会继续处理，直到对端关闭或完成释放；连接数随后自然归零。accept loop 在等待 `max_clients` 配额时也响应停止信号。`hosting::tests::stop_stops_accepting_without_dropping_existing_connections` 与 `stop_interrupts_a_connection_waiting_for_a_client_slot` 覆盖这两条路径。

### 2.18 【已修复】`DicomClient::cancel` 使用请求实际的 presentation context

位置：`client.rs:314-320`。

```rust
pub async fn cancel(&mut self, message_id: u16) -> Result<bool> {
    let Some(association) = self.association.as_mut() else { return Ok(false) };
    let Some((last_id, context_id)) = self.last_request else { return Ok(false) };
    if last_id != message_id { return Ok(false) }
    association.send_c_cancel(context_id, message_id).await?;
    Ok(true)
}
```

`Association::send_c_cancel`（`server.rs:1827-1840`）接受 `context_id` 参数，client 现在从 `last_request` 传入实际 context，C-GET 等使用非 1 context 的请求不会再错投。公开 client 仍是同步式 `send_*` API，无法在同一个 `&mut self` 借用期间启动并发取消；需要异步请求句柄时另行设计 API。

### 2.19 【低】`build_user_information` 零生产调用点，其测试给出虚假安全感

位置：`association.rs:400-413`（定义），`association.rs:681-684`（唯一调用点，在 `mod tests` 内，`mod tests` 起于 `association.rs:416`）。

grep 全 crate 确认：`build_user_information` 只出现两次——定义和测试。生产路径完全不经过它，而是**各自手工构造 `UserInformation` 结构体**：`server.rs:198-200`（SCP 发 AC）和 `server.rs:1046-1052`（SCU 发 RQ）。

后果有两层。其一，`association.rs:683-684` 的断言（`implementation_class_uid == Some("1.2.826.0.1")`、`implementation_version_name == Some("RADICOM_0_1")`）验证的是一个没人调用的函数，真实路径的 UID/version 填写（`server.rs:26-27` 常量）没有被任何断言覆盖。其二，这解释了 golden 的一个真实覆盖缺口：

`tests-data/goldens/associate_rq.hex` 是由 `goldens.rs:37-45` 的 `fixture_associate_rq()` 生成的，而该 fixture 只做 `Associate::new(...)` + push 一个 presentation context，**从未设置 `implementation_class_uid`**（最后一次写入只有 `00 50 00 00 08 51 00 00 04 00 00 40 00`，即仅 Max-PDU-Length=0x4000 一个子项，无 0x52/0x53/0x54/0x55 任何一项）。所以这条 golden 钉住的是一个**空用户信息**，`Implementation Class UID` 在真实 RQ 里的编码（`pdu.rs:649-653`）只有 `pdu.rs:784-796` 的往返测试覆盖，没有字节级 golden。

`tests/interop.rs` 也补不上：`self_interop_c_echo_over_tcp`（`tests/interop.rs:51`）与 `self_interop_c_store_real_file_round_trips`（`tests/interop.rs:67`）都是 radicom→radicom，两侧用同一套常量，实现一致性无法被证伪；`external_peer_c_echo_and_optional_c_store`（`tests/interop.rs:143`）在无 `RADICOM_PEER` 时直接返回（见 §4.3），CI 里从不执行。

判定：不是功能缺陷（真实路径确实填了 UID），而是**死函数 + 覆盖缺口**——删掉 `build_user_information` 让 `server.rs` 的两处复用同一构造器，或给它接线；同时补一条带 `implementation_class_uid` 的 RQ golden。

---

## 3. 性能问题

测量环境：release 构建，22 逻辑核，Windows，`Instant` 计时，探针在 `%TEMP%\netperf`（未入库）。所有数字为本轮实测，非引用。

### 3.1 【高】默认 C-STORE 落盘路径：约 4 倍吞吐损失 + 双份内存

位置：`store.rs:60-77` + `radicom/src/io/file.rs:200-237`。

`write_and_parse` 做三件事：`File::create` + `write_all` + `flush`（`store.rs:61-63`），然后**用内存里的 `bytes` 重新解析**（`store.rs:66` `DicomFile::from_bytes(bytes, ...)`）。也就是说文件写出去之后**再没读过它**——落盘纯属浪费；而 `store.rs:1-5` 的模块注释声称这样做是"so memory stays bounded for large instances"，实际上 `bytes` 本身已经在内存里（`assembler.rs:70` 攒出来的），落盘既不省内存，还多一次同步 IO。

实测（1.25 MiB/实例，40 次 C-STORE，loopback）：

| sink | 吞吐 |
| --- | --- |
| `Memory` | 241.7 MiB/s |
| `SpoolToTemp`（默认） | 167.4 MiB/s |
| `SpoolToDir` | 109.1 MiB/s |

小实例（65 element / 约 1 KB）差距更悬殊，因为固定开销占主导：

| sink | 吞吐 | 单次耗时 |
| --- | --- | --- |
| `Memory` | 5058.9 stores/s | 197.7 µs |
| `SpoolToTemp`（默认） | 1259.7 stores/s | 793.8 µs |
| `SpoolToDir` | 1150.8 stores/s | 869.0 µs |

即**默认配置比可选配置慢 4 倍**；文件生命周期泄漏已由 §2.2 的 `TempSpool` 修复。性能上的最小修法仍是让 `SpoolToTemp` 直接走 `Memory` 的解析路径，把落盘只留给真正需要持久化的 `SpoolToDir`。

### 3.2 【高】同步文件 IO 直接在 async 任务里执行，无 `spawn_blocking`

位置：`store.rs:51`、`store.rs:61-63`。

`std::fs::create_dir_all`、`std::fs::File::create`、`write_all`、`flush` 都是阻塞调用，直接跑在 tokio worker 线程上（`server.rs:687` 调用 `materialize`）。全仓 grep `spawn_blocking`/`block_in_place` **零命中**。大实例 C-STORE 期间 worker 线程被文件 IO 占住，同 runtime 上其它连接的 PDU 读取被推迟。这是"一个连接一个 task"模型下最容易被忽略的一类退化。

### 3.3 【高】解析器全局锁导致服务端并发完全不随核数扩展

这一项的病根在 `radicom` 解析器，但**服务层是暴露点**：每连接一个 task 是并发模型，却被解析串行化吃掉。

独立基准（同一内存 dataset，固定总工作量分摊到 N 线程，`DicomFile::from_bytes`，5 次取最优）：

| 线程数 | 800 element 集 wall | 吞吐 | 相对 1 线程 |
| --- | --- | --- | --- |
| 1 | 80.9 ms | 4942 parses/s | 1.00x |
| 2 | 49.0 ms | 8157 parses/s | 1.65x |
| 4 | 33.8 ms | 11835 parses/s | 2.39x |
| 8 | 30.7 ms | 13038 parses/s | 2.64x |

22 核机器，8 线程只拿到 2.6 倍。小数据集（65 element）情况类似：12.1 → 4.9 ms（4.9 → 8 线程几乎不再改善）。

把两个可疑的全局锁单独拿出来打点（同样固定总工作量，并附一个**无锁对照**）：

| 被测操作 | 1 线程 | 2 线程 | 4 线程 | 8 线程 |
| --- | --- | --- | --- | --- |
| `tag_index::lookup_interned`（`tag_index.rs:27` 全局 `RwLock` 读） | 19.6 ns | 65.6 ns | **106.4 ns** | **112.6 ns** |
| `default_dictionary()`（`dictionary.rs:233-245` `RwLock` 读 + `Arc` clone） | 20.6 ns | 30.8 ns | **74.7 ns** | **101.7 ns** |
| 对照：线程本地 `HashMap` 查找（无共享锁） | 52.8 ns | 26.1 ns | 18.4 ns | 11.2 ns |

对照行随线程数**加速**（52.8 → 11.2 ns），两个 radicom 操作随线程数**退化**（19.6 → 112.6、20.6 → 101.7 ns）。这排除了"CPU 降频 / 内存带宽 / 基准噪声"这类解释，确认就是锁竞争：单条 cache line 上的 `RwLock` 读让所有解析线程互相踩。

病灶位置：

1. `crates/radicom/src/model/tag_index.rs:12-21` 的 `static INDEX: Lazy<RwLock<HashMap<u32, Arc<Tag>>>>`，`tag_index.rs:27` 每次 `INDEX.read()`，经 `reader.rs:281` 每 element 命中一次。表是**只读**的（`tag_index.rs:23-24` 注释自认"Unknown tags ... are not inserted"），所以 `RwLock` 只是白白引入争用。
2. `crates/radicom/src/model/dictionary.rs:233-245` 的 `default_dictionary()` 每次 `DEFAULT.read()` 再 `Arc::clone`，被 `reader.rs:195` 每 element 的 VR 解析调用。`set_default_dictionary` 是一次性替换（`dictionary.rs:247-258`），常见路径根本不需要锁。

服务层实测（`DicomServer::run` + N 个并发 `DicomClient`，每客户端 60 次 C-STORE，802 element 实例）：

| 并发客户端 | `Memory` sink | 相对 1 客户端 | `SpoolToTemp` | 相对 |
| --- | --- | --- | --- | --- |
| 1 | 3126 stores/s | 1.00x | 963 stores/s | 1.00x |
| 2 | 4174 | 1.33x | 1593 | 1.65x |
| 4 | 6095 | 1.95x | 2418 | 2.51x |
| 8 | 6807 | **2.18x** | 2484 | **2.58x** |

**对 DICOM 服务器的直接后果（本层最重要的一条性能事实）**：`DicomServer` 服务并发关联时**用不上多核做解码**。加客户端只会让每个客户端的解析延迟一起变差，而不是提高总吞吐；22 核机器上 8 并发在 2.2 倍处饱和。这不是微优化——它决定了这台 SCP 的容量上限，且服务层自身无法绕过（不可能把解析从接收路径上摘掉）。修法在 `radicom` 数据模型侧（只读表用免锁的 `OnceCell`/`&'static [T]`，或分片 map），服务层无需改动。

### 3.4 【高】文件式解析比内存式慢一个数量级，根因是每元素 3 次 seek

位置：`crates/radicom/src/io/byte_source.rs:66-77`、`crates/radicom/src/io/file.rs:112-170`。

实测（同一实例，`DicomFile::open` vs `DicomFile::from_bytes`）：

| element 数 | 字节 | `open` | `from_bytes` | 比值 |
| --- | --- | --- | --- | --- |
| 5 | 100 | 0.1093 ms | 0.0013 ms | 84x |
| 32 | 532 | 0.5418 ms | 0.0077 ms | 70x |
| 65 | 1060 | 0.9551 ms | 0.0145 ms | 66x |
| 302 | 5252 | 4.2313 ms | 0.0849 ms | 50x |
| 800 | 14216 | 13.0716 ms | 0.2090 ms | 63x |

注意 `open` 的成本几乎**纯线性的每元素成本**（约 14–17 µs/element，见上表最后一行推算），固定开销（打开文件、读 132 字节头、解析 group 0002）只有约 0.03 ms。所以这不是"文件比内存慢"这种模糊说法，而是**每个 element 都在做 syscall**。

机制（按 `reader.rs` 实际调用点核对）：每个 element 走 `require()` 三次以上，而 `require`（`byte_source.rs:66-77`）的实现是

```rust
let pos = self.inner.stream_position()?;      // syscall
let end = self.inner.seek(SeekFrom::End(0))?; // syscall
self.inner.seek(SeekFrom::Start(pos))?;       // syscall
if end - pos < n { ... }
```

`reader.rs` 的调用点：`reader.rs:171`（`position`）、`reader.rs:224-225`（`position` + `require(PROBE)`）、`reader.rs:248`（`require(length)`）、`reader.rs:299`、`reader.rs:360-362`、`reader.rs:380`、`reader.rs:415`、`reader.rs:470-473`。`file.rs:114` 和 `file.rs:160` 用裸 `std::fs::File` 包装，没有缓冲层，所以每次 `stream_position`/`seek` 都是真 syscall。

我用仪器化的 syscall 计数（与 Lead 的观测一致）回放这套模式并分别验证两种修法（65 element，纯 syscall 序列回放）：

| 变体 | 65 element 回放耗时 | 相对 as-is |
| --- | --- | --- |
| as-is（`stream_position`=860、`seek(End)`=295、其它 seek=295） | 0.9890 ms | 基准 |
| **只缓存文件长度**（去掉 `seek(End)`） | 0.5774 ms | **+33.7%** |
| **缓存长度 + 缓存当前位置**（去掉全部 seek） | 0.2688 ms | **+69.1%** |
| 加 `BufReader`（其余不变） | 1.2794 ms | −46.9%（更慢，**仅限本回放形状**，见下） |

**关于 `BufReader` 的结论必须限定范围**：它不是有效修法，且效果随文件而反转。在**真实解析路径**上（`ByteSourceReader` + `DicomReader` + `DatasetBuilder`，即 `open()` 实际做的事），Lead 实测 GH178.dcm 快 1.58x、ct.dcm 基本持平 1.02x、mr.dcm 反而慢 0.86x。上表最后一行的 −46.9% 只对"每元素约 1 次小读"的回放形状成立，不能外推。

控制变量是**每元素的小读次数**，不是 `BufReader` 本身：

| 回放形状 | raw | `BufReader` | 结果 |
| --- | --- | --- | --- |
| 每元素 1 次小读（上表形状） | 198.7 µs | 229.1 µs | **−13.3%（更慢）** |
| 每元素 4 次小读 | 519.9 µs | 214.7 µs | **+142.1%（快得多）** |

真实解析器把 tag、VR、length、value header 分开读，所以落在两者之间，并随文件大小摆动：小文件（每字节对应的元素头更多）受益明显，中等文件持平，大文件里绝大多数字节走 bulk value 读取、绕过缓冲区，因此反而变慢。

因此真正的修法是让 `ByteSourceReader` 自己维护位置与总长度（构造时 `metadata().len()` 拿一次），彻底不产生 seek——即上表 +33.7% / +69.1% 那两行；**而不是加缓冲层**。加缓冲既不稳定（同一改动在不同文件上快慢反转），也会把大文件拖慢。

好消息：`store.rs:66` 用的是 `from_bytes`，所以当前 C-STORE 路径**尚未**吃到这 10–80 倍。但它同时意味着 §3.1 的落盘是一次纯粹的浪费——一旦有人"优化"成 `write_and_parse` 里改用 `DicomFile::open`（看起来更自然），接收吞吐会立刻塌到 1/50。这条应作为"不要改成 open"的显式警告保留。

### 3.5 【已修复】三种流式查询/检索服务的 C-CANCEL

位置：`server.rs:567-757`。

C-FIND、C-MOVE 与 C-GET 现在都逐条消费 provider stream，首个 Pending 响应可在 provider 产生终端响应前到达 SCU。三条处理路径都会同时读取入站 P-DATA，在同一 presentation context 和 message ID 收到 C-CANCEL 时回送 `CANCEL` 终止响应并丢弃 provider stream；C-GET 在反向 C-STORE 已发出时会先消费对应的 C-STORE-RSP，再结束原检索操作。`loopback_c_find_c_cancel_stops_an_in_flight_provider`、`loopback_c_move_c_cancel_stops_an_in_flight_provider`、`loopback_c_get_c_cancel_stops_an_in_flight_provider` 和 `loopback_c_get_c_cancel_during_reverse_store_preserves_progress` 覆盖这些路径。

### 3.6 【中】C-GET 每实例重新构造 `Transcoder` 并整体转码

位置：`server.rs:823-832`。

```rust
let payload = if source_syntax.uid() == target.uid() {
    crate::message::encode_dataset(dataset, &target)?
} else {
    let transcoder = Transcoder::new(source_syntax, target.clone(), None, None);
    let transcoded = transcoder.transcode_dataset(dataset)?;    // 每个实例一遍
    crate::message::encode_dataset(&transcoded, &target)?
};
```

同质实例（同一 SOP class、同一语法）会重复构造 `Transcoder`、重复 resolve 编解码器。一个 500 张的 study 就是 500 次。最小修法：把 `Transcoder` 按 `(source, target)` 缓存在 `Serving` 里（`serving` 生命周期正好覆盖整次 C-GET）。

### 3.7 【中】`PdvAssembler` 逐 PDV `extend_from_slice`，无零拷贝

位置：`assembler.rs:70`。

```rust
buffer.extend_from_slice(&pdv.data);
```

`pdv.data` 本身就是 `Vec<u8>`（`pdu.rs:243`），`decode_pdata` 已经把它从帧里拷出来了，这里再拷一遍进累积缓冲。1 MiB 实例 = 完整两次内存复制（帧 → PDV → 消息）。`bytes::Bytes` 已是依赖（`Cargo.toml`），改成 `Vec<Bytes>` 收集再一次性 `copy_from_slice` 到紧凑缓冲，可把中间累积的扩容拷贝消掉。

### 3.8 【中】C-MOVE 逐实例串行 C-STORE，无流水线

位置：`cmove.rs:314-317`。

```rust
for file in files {
    let response = association.send_c_store_with_originator(file, originator_ae, originator_id).await?;
    summary.record(response.state());
}
```

一次 study 的 N 个实例一个一个发，每个都等响应。RTT 100 ms 的网段上，100 张图 = 10 秒纯等待。关联已经建好（`cmove.rs:309`），完全可以在协商窗口内并发（PS3.7 §9.3 允许 N 个在途请求，受异步操作窗口限制；本库默认 (1,1) 窗口，但至少可以把窗口提上去）。

### 3.9 【低】`collect_reverse_stores` / 事件 sink 用 `DicomFile::clone` + `std::sync::Mutex`

位置：`server.rs:1166-1171`（`request.file.clone()`）、`client.rs:108-114`（`request.file.clone()`）。

C-GET 收到 500 张图就会深拷贝 500 个 dataset 进 `Vec`。虽然 `DicomFile: Clone`（`radicom/src/io/file.rs:73-74`），但穿过 `Mutex` 的锁竞争加上深拷贝，在高并发接收时是实打实的浪费。这张表存在的主要用途是测试便利（`server.rs:2512-2520`），生产用途不明。

---

## 4. 测试审查

本节保留首次走查时的测试缺口快照；已修复的跨 SOP 类重协商、请求级超时、PDV 交错、C-FIND/C-MOVE/C-GET 流式取消和 N-service 回环条目仅用于追溯，当前状态以 §1、§2 和源码为准。

328 个单元测试全绿，但对服务层的关键行为覆盖仍有缺口，且至少有 4 个测试是**假绿**（断言与名字/意图不符，或在不该通过的条件下通过）。

### 4.1 【高】`a_c_cancel_keeps_the_association_healthy` 是假绿

位置：`client.rs:550-583`。

测试流程：发 C-ECHO → 拿响应 → `cancel(1)` → 再发 C-ECHO → 断言"关联仍健康"。但它取消的 message ID 1 **响应早已收到**，因此仍没有断言取消产生任何效果。新增的 `server.rs::loopback_c_find_c_cancel_stops_an_in_flight_provider` 使用低层 `Association` 在 provider 永久等待时发送 C-CANCEL，并覆盖 Identifier 与 C-CANCEL 在同一 P-DATA-TF 的排队场景，断言 SCU 收到 `CANCEL` 终止响应。

### 4.2 【已修复】临时 C-STORE 文件测试覆盖

`store.rs:205-218` 的 `spool_to_temp_removes_its_file_with_the_instance` 现在从 `ReceivedInstance.temp` 取得实际路径，断言物化期间文件存在、实例 drop 后文件消失；这对应 §2.2 的生命周期修复。

### 4.3 【高】`external_peer_c_echo_and_optional_c_store` 在 CI 里静默通过

位置：`tests/interop.rs:143-155`。

```rust
let Ok(peer) = std::env::var("RADICOM_PEER") else {
    eprintln!("skip: set RADICOM_PEER=host:port to run interop against a real peer");
    return;          // ← 报告为 "ok"
};
```

未设环境变量时直接 `return`，测试报告 `ok`。这在 CI 里等价于"互操作永远通过"。同时它还带 `?optional` 语义（名称里的 `and_optional_c_store`），即使连上真对端也可以选择不测 C-STORE。真正需要的是 `#[ignore]`（显式忽略，报告为 ignored 而非 passed）或独立的 interop job。

### 4.4 【历史快照】`watchdog_reports_expired_requests` 曾被误判为未接线代码

位置：`engine.rs:375-390`。

首次走查时 `PendingTable` 只有模块测试，容易被误判为未接线；当前 `Association` 用它分配
在途 Message ID，并由 `request_timeout` 包裹响应等待。`PendingTable::expired` 仍不是独立的
多请求 watchdog 扫描；§1.2 已将其定性为 ⛔ 不适用（同步 API 每次只等一个响应，无需独立扫描）。

### 4.5 【中】`loopback_honours_a_small_peer_max_pdu` 只断言 C-ECHO 成功

位置：`server.rs:2764-2778`。

```rust
async fn loopback_honours_a_small_peer_max_pdu() {   // 注释说 "a 64-byte maximum forces the command set into several PDVs"
    ...
    assert!(association.send_c_echo("1.2.840.10008.1.1").await.is_ok());
}
```

断言只有"echo 成功"。PDU 是否真的没超过 64 字节、是否真的被切成多个 PDV、对端是否按 PDV 边界正确重组——一条都没测。既然注释描述了期望，就该断言收到的 P-DATA 数量 > 1 或每个 `Pdu::PData` 的编码长度 ≤ 64。

### 4.6 【已修复】C-GET 进度与反向 C-STORE 覆盖

`c_get_reports_the_remaining_sub_operations` 使用三个实例，断言 SCU 看到 `remaining = 3, 2, 1`、终止值为 0，并校验三个反向 C-STORE Message ID 唯一；流式 Pending、provider 失败和计数合并另有专门回归测试。

### 4.7 【中】仍待补齐的关键路径覆盖

当前仍待补齐的覆盖如下：

| 覆盖缺口 | 当前状态 |
| --- | --- |
| N-EVENT-REPORT / N-GET / N-SET / N-CREATE | 已有 `server.rs:3728-3743` 的真实回环测试 |
| `HostedDicomServer` stop 后再 start | `hosting.rs:326` 的 `stop_then_start_serves_again` 已覆盖 |
| 畸形 P-DATA 中途（已建立关联后） | `server.rs:4064` 的 socket 级回归覆盖；随机 fuzz 仍只覆盖 PDU 解码 |
| 同 client 跨 SOP 类复用 | `client.rs:738` 的 `a_second_sop_class_gets_its_own_association` 已覆盖当前重开关联语义 |
| C-FIND 的 C-CANCEL | `server.rs::loopback_c_find_c_cancel_stops_an_in_flight_provider` 已覆盖在途 provider 与同 P-DATA-TF 排队场景 |
| C-MOVE 的 C-CANCEL | `server.rs::loopback_c_move_c_cancel_stops_an_in_flight_provider` 已覆盖在途 provider |
| C-GET 的 C-CANCEL | `server.rs::loopback_c_get_c_cancel_stops_an_in_flight_provider` 和 `loopback_c_get_c_cancel_during_reverse_store_preserves_progress` 已覆盖 provider 等待与反向 C-STORE 等待 |

### 4.8 【低】`tests/goldens.rs` 只覆盖 2 个 PDU

位置：`tests/goldens.rs:67`。只校验 `associate_rq.hex` 与 `pdata_cecho.hex`，服务层的命令集字节（§2.15）、C-GET 反向 C-STORE 命令集、N-service 命令集都无 golden。

### 4.9 【低】benchmark 只测了非默认路径

位置：`benches/throughput.rs:77`（`store_sink: StoreSink::Memory`）。实测该 bench 输出 `269.1 MiB/s`，但它绕开了默认的 `SpoolToTemp`——而后者实测只有 167.4 MiB/s。也就是说 **benchmark 报告的吞吐比生产默认配置高 60%**，且不会暴露 §3.1 的落盘浪费。

### 4.10 【低】`build_user_information` 的测试验证的是死函数；golden 钉住空用户信息

位置：`association.rs:681-686`（测试），`goldens.rs:37-45`（fixture）。

`association.rs:683-684` 断言 `implementation_class_uid`/`version_name`，但被测函数零生产调用点（详见 §2.19），真实路径（`server.rs:199`、`server.rs:1048`）绕开它。同时 `associate_rq.hex` 的用户信息只含 Max-PDU-Length 一项，`0x52 Implementation Class UID` 在真实 RQ 中的字节编码没有任何 golden 或互通测试覆盖——`self_interop_*`（`tests/interop.rs:51`、`:67`）两侧同源无法证伪，`external_peer_*`（`:143`）在 CI 里从不执行。

### 4.11 值得肯定的部分

- `server.rs:1902-1923`（`loopback_abort_ends_the_connection`）、`server.rs:2613-2662`（per-SOP-class 扩展协商）、`server.rs:2664-2743`（user identity 三种策略 + 必需项缺失）都是真断言、真路径。
- `store.rs:205-213`（`wire_syntax_overrides_the_parsed_one`）抓了一个容易被忽略的语义点。
- `a_required_identity_response_that_is_missing_fails_the_association`（`server.rs:2719`）证明了作者确实考虑过负路径。

---

## 5. 结论与优先级

按本层证据排。第 1 项单独说明其严重性边界。

| 顺序 | 问题 | 位置 | 级别 | 修法规模 |
| --- | --- | --- | --- | --- |
| 1 | 对端可触发 panic（默认连接级；`panic="abort"` 下进程级 remote DoS） | `dimse.rs:225` ← `server.rs:354-359` | **已修复** | 协议错误路径返回 Provider A-ABORT；响应型 command 保留 User-source A-ABORT |
| 2 | 默认 sink 临时文件泄漏 | `store.rs:68-80` | **已修复** | `TempSpool` 在 drop 时删除并有回归测试 |
| 3 | C-MOVE 终端计数归零 | `server.rs:727-757` | **已修复** | 用累加 summary 兜底，并有回归测试 |
| 4 | `max_clients` 不生效 | `server.rs:179-210` | **已修复** | Semaphore 门控 accept loop，并有并发槽位测试 |
| 5 | 服务端无 ACSE 超时 | `server.rs:269-281` | **已修复** | 首个 PDU 读取超时并发送 Provider A-ABORT |
| 6 | C-GET `remaining` 语义 | `server.rs:567-665` | **部分修复** | Pending 改由 provider 驱动；未提供 Pending 时无法推导总数 |
| 7 | 客户端 PC 不复用 | `client.rs:149-155` | 中 | 中（需重开关联） |
| 8 | 默认 C-STORE 慢 4 倍 + 阻塞 IO | `store.rs:60-77` | 中高 | 小（走内存路径 + `spawn_blocking`） |
| 9 | 请求超时体系缺失（engine 死代码） | `engine.rs`、`client.rs:60` | 中 | 大（需设计接线） |
| 10 | 解析器全局锁限制服务并发 | `tag_index.rs:27`、`dictionary.rs:233` | 中高 | 大（跨 crate，但值得） |
| 11 | 文件式解析每元素 3 次 seek（勿切 `open`） | `byte_source.rs:66-77`、`file.rs:114` | 中 | 中（缓存长度+位置；**不要**加缓冲层） |

原始评审中的前 5 项已有第 1、2、3、4、5 项修复证据；当前仍需处理的是公开 client 的并发取消 API、客户端 PC 复用、请求 watchdog 和默认 sink 的落盘性能。第 2 项的生命周期修复与第 8 项性能问题已经分开，不能用性能优化替代清理保证。

第 1 项的严重性要说准：**当前默认配置下它是连接级**（panic 在 `tokio::spawn` 的连接 task 内被捕获，accept loop 存活、新客户端仍可连），所以不是开箱即用的 remote DoS；但触发载荷只有 64 字节、无需任何权限、且打掉的连接上所有在途请求静默丢失且不发 A-ABORT。一旦部署侧启用 `panic = "abort"`（服务端二进制的常见选择），它立刻变成进程级 DoS。因此修复优先级按"最高"排，而不按"当前影响面小"降级。

第 6、7、9 项属于设计取舍，需要先决定 API 语义再动代码。第 10、11 项不在本 crate 内，但服务层是直接受害者，建议与 io/数据模型评审一并排期；第 11 项的结论是双重的——"保持现状用 `from_bytes`，不要改用 `open`"属防守性约束，而若真要修 `open`，修法是缓存长度与当前位置消除 seek，**不是**加 `BufReader`（后者在真实路径上 GH178 快 1.58x、ct 持平、mr 慢 0.86x，效果随文件反转）。
