# 网络协议层评审（`radicom-network` 协议层）

评审对象：`crates/radicom-network/src/` 下协议层文件 —— `pdu.rs`、`transport.rs`、`tls.rs`、`association.rs`、`dimse.rs`、`message.rs`、`extended.rs`、`nservice.rs`、`print_sop.rs`、`error.rs`、`lib.rs`。

参照实现：fo-dicom `FO-DICOM.Core/Network/`（`PDU.cs`、`DicomService.cs`、`DicomStatus.cs`、`DicomPresentationContext.cs`、`DicomMessage.cs`、`DicomServiceApplicationInfo.cs`、`DicomUserIdentityNegotiation.cs`、`Tls/*.cs` 等），以及 DICOM PS3.8（Upper Layer Protocol）、PS3.7（DIMSE）、PS3.4（Service Class）。

方法：逐行阅读上述 Rust 源码，与 fo-dicom C# 的写/读两侧逐字段比对；只在读完实际代码并确认调用点可达后给出结论。repo 内 `docs/*.md` 的自评不作为依据。本文件同时保留原始发现和后续修复状态。

### 证据强度

本报告对每条结论标注证据强度：

| 标注 | 含义 |
| --- | --- |
| **字节验证** | 已用按 PS3.8/PS3.7 手工构造的标准字节喂给 `pdu::decode`，以及取 `pdu::encode` 的输出做逐字节比对，真实复现了错误行为。强证据。 |
| **源码比对** | 逐字段比对 radicom 与 fo-dicom 两侧读写代码，未构造字节。结论正确性依赖比对本身。 |

按标准生成字节的生成器与探针放在系统临时目录（未进入仓库）：`<tmp>/oracle/mkconformant.py` 生成 fixture，`<tmp>/oracle/confcheck/` 验证解码，`<tmp>/oracle/confform/` 验证编码。原始 §2.1–§2.5 条目附有关键字节与复现输出，当前回归证据以源码测试为准。

**历史基线**：workspace 构建通过、`clippy` 无告警、`radicom-network` 248 个测试通过（由 Lead 统一验证）。测试审查章节保留这次历史快照，当前状态以源码和本轮结构检查为准。

**当前结论**：A-ASSOCIATE 的 presentation context、Role Selection、SOP Class Extended Negotiation 和 User Identity 线格已按 PS3.8/PS3.7 修复并有字节级回归。当前协议层仍有有界 Max-PDU、异步操作窗口未接入引擎、状态码按 service class 的描述表缺失等差异；这些差异见 §1 和 §2。

**历史共性**：原始四类线格问题无法被"自往返"测试发现，只能靠外部互通测试或标准派生 golden 字节捕获；当前 `pdu.rs` 已加入对应的手工字节回归。

本次评审共发现：

| 级别 | 数量 | 证据强度 | 说明 |
| --- | --- | --- | --- |
| 高 | 6（历史） | 原始 §2.1–§2.5 字节验证 | 原始评审发现；四类线格问题已修复，Max-PDU 保留有界设计 |
| 中 | 8 | 源码比对 | 边界/健壮性/规范偏差 |
| 低 | 7 | 源码比对 | 命名、效率、可维护性 |

---

## 1. 功能缺失（对比 fo-dicom，仅协议层）

> **状态提示**：本报告是首次对齐走查的**历史快照**，正文以"当时未修复"的口吻记录问题；所列缺陷/缺失大多已在后续提交修复，当前能力与状态请以 §1 表格（✅/⛔）及 `PROGRESS.md` 为准。

状态取值：**已实现** / **部分实现** / **缺失** / **有意不实现**。"缺失"仅在已 grep 确认 repo 内无对应实现后标注。

> 判定口径：**以功能是否达成为准**，不要求与 fo-dicom 同名的 .NET 类/接口/结构一一对应。Rust 按其生态
> 惯例实现（tokio + socket2 + rustls，具体类型而非 `INetworkManager`/`DicomTimeout` 等 .NET 抽象）；
> 功能等价即视为已实现，.NET 专有抽象在 Rust 无必要者标“不适用/有意不实现”。

### 1.1 PDU 编解码

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 7 种 PDU 类型 | `PDU.cs:RawPduType` | 已实现 | `pdu.rs:262` `decode` / `pdu.rs:563` `encode` | 0x01–0x07 全覆盖 |
| 6 字节 PDU 头（type/reserved/len BE） | `PDU.cs:RawPDU.CommonFieldsLength` | 已实现 | `transport.rs:12` `HEADER_LEN`、`transport.rs:35` `read_pdu` | 读 1 字节再读 5 字节，可正确处理 EOF |
| A-ASSOCIATE-RQ 编解码 | `PDU.cs:547` | **已实现** | `pdu.rs:678` `encode_associate`、`pdu.rs:329` `decode_associate` | 0x54/0x56/0x58 按 PS3.8 线格编码，手工 wire fixture 覆盖 |
| A-ASSOCIATE-AC 编解码 | `PDU.cs:915` | **已实现** | 同上（`is_rq=false`） | presentation context result 位于 item body 偏移 2，字节级回归覆盖 |
| A-ASSOCIATE-RJ 编解码 | `PDU.cs:AAssociateRJ` | 已实现 | `pdu.rs:514` `decode_associate_rj`、`pdu.rs:567` | 4 字节体，符合 §9.3.4 |
| P-DATA-TF 编解码 | `PDU.cs:1569` `PDataTF` | 已实现 | `pdu.rs:524` `decode_pdata`、`pdu.rs:716` `encode_pdata` | PDV 位定义、偶长填充均正确 |
| A-RELEASE-RQ/RP | `PDU.cs` | 已实现 | `pdu.rs:569` | — |
| A-ABORT | `PDU.cs` | 已实现 | `pdu.rs:552` `decode_abort`、`pdu.rs:570` | source 语义见 §2.9 |
| 16 位 item 长度回填 | `PDU.cs` `MarkLength16` / `WriteLength16` + `Stack<long>` | 有意不实现 | `pdu.rs:645` `push_item` | Rust 侧直接算好长度后整体写出，无需回填；长度超出 u16 会返回协议错误，见 §2.6 |

### 1.2 用户信息子项（User Information，item 0x50）

| 子项 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 0x51 Max-PDU-Length | `PDU.cs:596-600` | 已实现（有界） | `pdu.rs` 解码 / 编码 | 通告值钳到 `MAX_PDU_SIZE=262144`（256 KiB）；`0`（unlimited）也落到该上限，见 §2.5 |
| 0x52 Implementation Class UID | `PDU.cs:602-607` | 已实现 | `pdu.rs:401` / `pdu.rs:649` | — |
| 0x53 Asynchronous Operations Window | `PDU.cs:609-617`（仅在非 (1,1) 时发送） | ⛔ 有意不实现（每关联串行） | `pdu.rs` 编解码 | 编解码正确、默认省略正确；radicom 的关联 API 为 `&mut self` 串行操作、服务端按序处理单个请求，恒协商 (1,1)（ACK 省略 0x53），因此不声明并发窗口，属有意设计而非缺陷 |
| 0x54 SCP/SCU Role Selection | `PDU.cs:619-634` | **已实现** | `pdu.rs:445-466` 解码 / `pdu.rs:732-744` 编码 | `[uid_len][uid][scu][scp]` 线格和手工字节测试均正确 |
| 0x55 Implementation Version Name | `PDU.cs:636-641` | 已实现 | `pdu.rs:404` / `pdu.rs:652` | — |
| 0x56 SOP Class Extended Negotiation | `PDU.cs:643-654` | **已实现** | `pdu.rs:480-538` / `pdu.rs:752-762` | application information 使用扁平字段数组，RQ/AC 方向有回归覆盖 |
| 0x57 SOP Class Common Extended Negotiation | `PDU.cs:656-677` | 已实现 | `pdu.rs:432` / `pdu.rs:683-694` | 仅 RQ，符合 §9.3.2.3 |
| 0x58 User Identity Negotiation (RQ) | `PDU.cs:679-694` | **已实现** | `pdu.rs:490-517` / `pdu.rs:780-793` | type/positive/length/payload 顺序按 PS3.8 编解码，字节级回归覆盖 |
| 0x59 User Identity Negotiation (AC) | `PDU.cs:1017-1024` | 已实现 | `pdu.rs` 解码/编码 | AC 项体按 PS3.8 §D.3.3.7 为 `[server-response-length u16][response bytes]`，长度越界报协议错误；有字节级回归 |
| 未知子项按长度跳过 | `PDU.cs:1164` `SkipBytes` | 已实现 | `pdu.rs:455`、`pdu.rs:345` | — |

### 1.3 关联协商与策略

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| presentation context ID 奇数分配 1..255 | `DicomPresentationContextCollection.cs:GetNextPresentationContextID` | 已实现 | `association.rs:89` `ContextIdAllocator` | 255 后正确耗尽 |
| 重复 context 去重 | `DicomPresentationContextCollection`（按 abstract+roles+TS 集合） | 已实现 | `association.rs:125` `is_duplicate_of` | TS 集合排序后比较，角色按 SOP class 排序比较，语义正确 |
| 抽象语法优先于传输语法判定 | `DicomService` 协商逻辑 | 已实现 | `association.rs:292` `negotiate_one` | 不支持抽象语法 → 3（RejectAbstractSyntaxNotSupported） |
| 传输语法接受顺序（Offered / Preferred） | fo-dicom 无同名开关，按 propose 顺序取首个匹配 | 已实现（超出参照） | `association.rs:304` `SyntaxPriority` | 提供两种策略，默认应与 fo-dicom 对齐（取 offered 顺序） |
| 结果码 0–4 | `DicomPresentationContext.cs:DicomPresentationContextResult` | **已实现** | `association.rs:49` `result_code` | `Proposed` 返回 `None`，AC 编码不会把未协商 context 当成 Accept，见 §2.10 |
| 角色选择授予策略 | `PDU.cs` + `DicomPresentationContext.UserRole/ProviderRole` | 已实现 | `association.rs:576` 测试覆盖、`association.rs:341` | 策略和 0x54 wire layout 均已覆盖 |
| 已接受 context 的路由选择 | `DicomService` `_assoc.PresentationContexts.FirstOrDefault` | 已实现 | `association.rs:357` `AcceptedContexts::pick` | 精确 TS 优先，回退首个（`association.rs:622-663` 有覆盖） |

### 1.4 DIMSE 命令集

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 22 个 Command Field | `DicomCommandField.cs` | 已实现 | `dimse.rs:104` `code` / `dimse.rs:136` `from_code` | 逐个核对，与 fo-dicom 完全一致 |
| 请求/响应 bit15 判定 | `DicomMessage.cs:IsRequest` → `((int)type & 0x8000) == 0` | 已实现 | `dimse.rs:160` 附近 `is_request` | — |
| 命令集恒为 Implicit VR LE | `DicomService` 命令编码路径 | 已实现 | `message.rs:16` `encode_command` | 显式固定 `IMPLICIT_VR_LITTLE_ENDIAN` |
| (0000,0000) 组长度重算 | `DicomMessage` 编码 | 已实现 | `dimse.rs:300-306` | 用 writer 算长度再减 12，有测试 `dimse.rs:580` |
| (0000,0800) Command Data Set Type | `DicomMessage.cs:127`（`0x0202` / `0x0101`） | 已实现 | `dimse.rs:44` `NO_DATA_SET`、`dimse.rs:257-262` | 用 `0x0001` 表示"有数据集"，PS3.7 允许（≠0101H 即可），但与 fo-dicom 的 `0x0202` 不同 |
| 优先级元素 | `DicomPriority.cs` | **已实现** | `dimse.rs:49-75` `Priority` | `Medium=0`、`High=1`、`Low=2`，缺失/未知值回落 Medium |
| 状态码分类 | `DicomStatus.cs:Lookup`（mask + 通配 `Bxxx`/`Cxxx`/`A7xx`） | 已实现 | `dimse.rs` `classify` / `status_pattern` | `classify` 先精确后 `Bxxx`/`Cxxx` 家族；`status_pattern` 返回 fo-dicom 风格通配串（`Bxxx`/`Cxxx`/`A7xx`/`A9xx`）。修正此前把 `0x0106` 等 DIMSE-N 失败码误判为 Pending 的问题（PS3.7 仅 `FF00`/`FF01` 为 Pending） |
| 状态码常量表 | `DicomStatus.cs:311-515` | 已实现 | `dimse.rs` `status` / `status_description` | 常量与描述覆盖 PS3.7 Annex C 的通用码（`0x0106`–`0x0123`、`0x0210`–`0x0213` 等）；`0xA900`/`0xA700` 多义性由 `status_description_for_service_class` 表达，见 §2.8 |
| 状态码 → 名称/描述 | `DicomStatus.Description`、`DicomStatus.ToString` | 已实现（可扩展） | `dimse.rs` `status_description*`、`register_status_description` | 内置 Generic/Storage/QueryRetrieve 措辞；`register_status_description` 支持应用注册私有/新状态描述（对应 `AddKnownDicomStatuses`） |
| 离线命令集解析容错 | `DicomMessage` 从 Dataset 构造 | 已实现 | `dimse.rs:311` `from_dataset` | 缺字段用 `get_or` 兜底 |

### 1.5 N-Service（DIMSE-N）

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 6 个 N 请求/响应字段 | `DicomCommandField.cs` | 已实现 | `nservice.rs:18` `N_REQUEST_FIELDS` | — |
| N-CREATE / N-EVENT-REPORT 用 Affected* | `DicomMessage.cs:74-76`、`DicomNCreateRequest.cs:45` | 已实现 | `nservice.rs:57-58` | — |
| N-GET / N-SET / N-ACTION / N-DELETE 用 Requested* | `DicomMessage.cs:64-68`、`DicomNGetRequest.cs:45`、`DicomNSetRequest.cs:24`、`DicomNActionRequest.cs:51`、`DicomNDeleteRequest.cs:24` | **已实现** | `nservice.rs:31` `uses_requested_uids`、`:73-84` | 请求按 field 写入 `(0000,0003)/(0000,1001)`；N-CREATE/N-EVENT-REPORT 仍用 Affected* |
| N-GET 的 Attribute Identifier List (0000,1005) | `DicomNGetRequest.cs` + `DicomTag.AttributeIdentifierList` | **已实现** | `nservice.rs:54-56`、`dimse.rs:34` | 请求支持 AT 列表，编码/解析和标准 tag 有回归测试 |
| N-SET 的 Modification List | PS3.7 §10.1.2 | 已实现 | `nservice.rs` `NServiceRequest::dataset` | N-SET 的修改列表就是请求携带的 Dataset：`NServiceRequest` 已有 `dataset: Option<Dataset>` 并参与编解码与 `no_dataset` 标志，无需独立 tag |
| N 响应回显 Affected SOP Instance UID | `DicomNGetResponse.cs` 等 | 已实现 | `nservice.rs:110-111`、`nservice.rs:124-125` | — |
| N 响应回显 Event/Action Type ID | `DicomNEventReportResponse.cs`、`DicomNActionResponse.cs` | 已实现 | `nservice.rs:112-113`、`nservice.rs:126-127` | — |
| N-CREATE-RSP 返回新建实例 UID | `DicomNCreateResponse.cs` | **已实现** | `nservice.rs` `NServiceResponse::success_with_new_instance` | 可携带 SCP 分配的新实例 UID；普通响应仍回显请求值 |

### 1.6 传输 / TLS / 平台（按 Rust 生态判定，不照搬 .NET 结构）

> 判定口径：fo-dicom 的若干传输/平台条目是 **.NET 特有抽象**（`NetworkStreamCreationOptions`、
> `INetworkManager`、`DicomTimeout` 枚举、Windows 证书库）。Rust 的等价做法是 tokio + `socket2`
> + rustls；因此本节按**能力**（是否达到同样效果）判定，不把同名 .NET API 当作缺口。

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| TCP_NODELAY | `NetworkStreamCreationOptions` | 已实现 | `transport.rs` `TcpOptions.nodelay` / `TcpStream::set_nodelay` | 默认开启 |
| SO_SNDBUF / SO_RCVBUF | `NetworkStreamCreationOptions.Send/ReceiveBufferSize` | 已实现 | `transport.rs` `apply_options`（`socket2::SockRef`） | tokio 无接收缓冲区 setter，故用 `socket2` 在底层 fd 上设置；内核拒绝时保持连接可用，有 socket 级回归测试 |
| TLS 服务端 / 客户端握手 | `Tls/DefaultTlsAcceptor.cs`、`Tls/DefaultTlsInitiator.cs` | 已实现 | `tls.rs` `accept` / `connect`（rustls） | 基于 tokio-rustls，服务端/客户端各自 `ServerConfig`/`ClientConfig` |
| 显式最低协议版本 | `SslProtocols.Tls12 \| Tls13` | 已实现 | `tls.rs` `TlsVersion` | 选择 TLS 1.2（兼容 1.3）或 TLS 1.3-only |
| 双向认证（mTLS） | `DefaultTlsAcceptor.RequireMutualAuthentication` | 已实现 | `tls.rs` `WebPkiClientVerifier`（配置 `client_ca_file`） | 客户端证书/私钥配对校验 |
| 证书校验开关 | `DefaultTlsInitiator.IgnoreSslPolicyErrors` | 已实现 | `tls.rs` `accept_any` | 语义等价，注释标明 "testing only" |
| SNI / 证书名指定 | `TargetHost` | 已实现 | `tls.rs` `server_name` | 显式名称优先，未配置时用已连接 peer IP |
| 证书格式支持 | PKCS#12 + 密码 / Windows 证书库 | ⛔ 有意不实现 | `tls.rs` `load_certs`/`load_key`（仅 PEM） | rustls 生态以 PEM（PKCS#8/PKCS#1）为准；PKCS#12/系统证书库需 openssl/平台 FFI 等重依赖，按需由调用方转为 PEM，符合轻依赖取向 |
| 超时 | `DicomTimeout.cs`（ACSE / DIMSE / Socket 三态枚举） | 已实现（Rust 表达） | `association_timeout`、`request_timeout`、`release_timeout` | Rust 不用常量枚举，而是各阶段具体的 `Duration` 字段；ACSE/DIMSE/释放各有超时与错误类型，Socket 空闲由 OS/TCP keep-alive 负责（不在每个读点重复叠超时） |
| 平台抽象层 | `INetworkManager`（Listener/Stream/IsSocketException/MachineName） | ⛔ 不适用 | `tokio::net::{TcpListener,TcpStream}`、`rustls` 的 `AsyncRead/AsyncWrite` | .NET 的 `INetworkManager` 是为替换 BCL 网络实现；Rust 侧直接依赖 tokio/rustls 的具体类型与 trait，无需再造一层 manager |
| 机器标识 | `NetworkManager.TryGetNetworkIdentifier`（MAC 追加到实现类 UID） | 已实现 | `print_sop.rs` `try_get_network_identifier`；`transport.rs` `network_identifier` | `transport::network_identifier()` 委托给 MAC 版本，无第二套语义 |
| 命令/数据缓冲上限 | `DicomServiceOptions.MaxCommandBuffer` / `MaxDataBuffer` | 已实现（等价） | `assembler.rs` `PduSplitter`；`transport.rs` `write_pdus`；`server.rs` `write_pdata_batch` | 分片仍按 `max_pdu`；一条 DIMSE 消息的分片 P-DATA 合并为**单次 `write_all`**（按消息攒批，减少 syscall），对齐 fo-dicom 的攒批意图 |
| 每 PDU PDV 数上限 | `DicomServiceOptions.MaxPDVsPerPDU` | 已实现 | `assembler.rs` `PduSplitter::new` | 参数形态一致 |

### 1.7 打印 / 其它

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| Meta SOP Class → 子类映射 | `DicomService.cs:1241` `MetaSopClasses.GetMetaSopClass` | 已实现 | `print_sop.rs:34` `instances` | 覆盖 Basic Grayscale / Color 两个 meta 类，与 fo-dicom 一致 |
| 反向查找（含 DefaultIfEmpty） | `MetaSopClasses.GetMetaSopClass` | 已实现 | `print_sop.rs:57` `meta_sop_classes_for` | 未命中时返回自身，语义对齐 |
| MAC → DICOM UID | `NetworkManager.TryGetNetworkIdentifier` | 已实现 | `print_sop.rs:76` | 十进制追加，合法 |
| Print SOP UID 常量 | `DicomUID` | 已实现 | `print_sop.rs:10` `uids` | — |

---

## 2. 正确性缺陷

### 2.1 【已修复】A-ASSOCIATE presentation context 的 wire layout

`decode_presentation_context` 和 `encode_associate` 统一使用
`ID(1) + Reserved(1) + Result/Reason(1) + Reserved(1)`。AC 的结果字节位于
item body 偏移 2，RQ 的该字节保持为 0；`presentation_context_encoder_writes_id_reserved_result_reserved`
和手工构造的 AC fixture 覆盖两个方向。

### 2.2 【已修复】Role Selection 子项（0x54）线格

角色选择按 PS3.8 D.3.3.4 编解码为
`SOP-class-UID-length + SOP-class-UID + SCU-role + SCP-role`。解码会校验 UID
和角色字段不越界，`role_selection_is_uid_length_then_uid_then_roles` 同时验证
标准输入和输出字节。

### 2.3 【已修复】SOP Class Extended Negotiation（0x56）

Service Class Application Information 现在是按字段号补齐的扁平字节数组，不再使用
自定义的 field/length/value 三元组。`fields_are_flat_and_gaps_are_zero_filled`、
`extended_negotiation_is_a_flat_field_array` 和 RQ/AC 方向测试固定了这一格式。

### 2.4 【已修复】User Identity Negotiation（0x58）

User Identity 按 PS3.8 D.3.3.7 编解码为
`type + positive + primary-length + primary + secondary-length + secondary`，
并对两个 payload 长度做边界校验；`user_identity_is_type_positive_then_length_prefixed_fields`
覆盖标准线格。

### 2.5 【已修复】Max-PDU 能力上限提升到 fo-dicom 的 256 KiB

`MAX_PDU_SIZE` 现为 `256 * 1024`（262144），与 fo-dicom `DicomServiceOptions.MaxPDULength`
默认值一致。P-DATA-TF 的长度字段本就是 32 位，旧的 65535 上限会拒绝合规对端发来的
大 PDU。`clamp_max_pdu_length` 仍把通告值钳到该上限，因此实现不会通告自己拒绝的值；
`read_pdu`/`decode_bytes` 用同一常量做接收门限。`a_pdu_at_the_advertised_max_is_accepted`
覆盖 256 KiB PDU 的接收，`advertised_max_pdu_is_clamped_to_what_we_can_receive` 覆盖
超限配置的钳位与未超限值的原样通告。

---

### 2.6 【已修复】`push_item` 的 u16 长度溢出

**位置**：`pdu.rs:645-657`

`push_item` 现在使用 `u16::try_from(value.len() + pad)`，单个 A-ASSOCIATE item 超过 65535 字节会返回 `NetworkError::Protocol`，不会截断长度字段。`encode_associate` 的所有 item 调用都传播这个错误；`pdu.rs` 的超长 item 回归覆盖该边界。

---

### 2.7 【已修复】`Priority` 数值映射与默认值

**位置**：`dimse.rs:47-73`、`dimse.rs:327`

当前实现按 PS3.7 使用 `Medium=0`、`High=1`、`Low=2`，并将缺失/未知值解析为 `Medium`。`dimse.rs` 的数值和默认值测试直接断言标准编码。

---

### 2.8 【中】`status` 常量用别名合并了语义不同的状态码

**位置**：`dimse.rs:483-508`

```rust
pub const OUT_OF_RESOURCES: u16 = 0xA700;
pub const OUT_OF_RESOURCES_2: u16 = 0xA701;
pub const IDENTIFIER_DOES_NOT_MATCH_SOP_CLASS: u16 = 0xA900;
pub const INVALID_ARGUMENT_VALUE: u16 = 0xA900;          // 与上一行同值
pub const STORAGE_FAILURE: u16 = 0xA700;                 // 与 OUT_OF_RESOURCES 同值
pub const STORAGE_OUT_OF_RESOURCES: u16 = 0xA700;
pub const REFUSED_OUT_OF_RESOURCES: u16 = 0xA700;
```

**为什么有问题**：fo-dicom 对同一数值在不同 service class 下给出**不同名称与描述**（`DicomStatus.cs:428-467`：Storage 侧 `A7xx` 是 "Out of Resources"，Query/Retrieve 侧 `A700` 是 "Out of Resources"、`A701` 是 "Unable to calculate number of matches"）。radicom 用 `_2` 后缀和大量别名把 `0xA701` 表达成 "OUT_OF_RESOURCES_2"，把 `0xA900` 同时叫 "identifier does not match" 和 "invalid argument value"。

后果：调用方无法从常量名得知语义；`STORAGE_FAILURE` 与 `STORAGE_OUT_OF_RESOURCES` 数值相同，而 PS3.4 中 C-STORE 的 `A700` 确实是"拒绝：资源不足"，但 `A801`/`A900` 在 Storage 与 Q/R 下含义不同。缺少"按 service class 解析"的能力。

**当前修复**：`StatusServiceClass` 与 `status_description_for_service_class` 已提供 `(code, service_class) -> description` 查询，覆盖 Storage 与 Query/Retrieve 对 `A900` 的不同语义；完整可扩展 `DicomStatus.Lookup` 注册表仍是后续差异。

---

### 2.9 【已修复】`AbortSource` 拒绝保留值

`AbortSource::from_code` 现在只接受标准的 source `0`（User）和 `2`（Provider）；保留值 `1` 及其它值返回 `None`，并由解码路径报告协议错误。`error.rs` 的回归测试覆盖该边界。

### 2.10 【已修复】`ContextState::Proposed` 不再伪装成 Accept

`ContextState::result_code` 现在返回 `Option<u8>`，`Proposed` 返回 `None`。AC wire 转换遇到未协商状态时使用拒绝码，避免把 proposed context 编码成接受；关联协商测试覆盖该行为。

### 2.11 【已修复】N-service 请求使用 Requested* 字段

`nservice.rs::uses_requested_uids` 按 PS3.7 §10.1 区分四个 Requested* 服务与 N-CREATE/N-EVENT-REPORT 的 Affected* 服务；N-GET 的 Attribute Identifier List 也已接入 `Command`。`nservice.rs` 现有 wire-level 测试逐项断言这些 tag。

### 2.12 【已修复】TCP socket buffer 选项实际生效

`transport::apply_options` 通过 `socket2::SockRef` 尝试设置 `SO_SNDBUF`/`SO_RCVBUF`；内核拒绝调优时保持连接可用。`socket_buffer_sizes_reach_the_socket` 读取 socket 选项验证了非默认配置确实生效。

### 2.13 【已修复】TLS 最低版本与 mTLS 配置

`TlsVersion` 显式选择 TLS 1.2（同时允许 TLS 1.3）或 TLS 1.3-only；服务端设置 `client_ca_file` 后使用 `WebPkiClientVerifier` 强制校验客户端证书。对应 TLS 回归覆盖版本下限、客户端证书和握手失败路径。

### 2.14 【已修复】TLS 默认证书名跟随连接目标

`TlsOptions::connect` 在未显式设置 `server_name` 时使用已连接 socket 的 peer IP；显式名称仍优先。代码和测试不再把 `localhost` 当默认校验名。

### 2.15 【已修复】AE title 按字节截断并保持 UTF-8 边界

`into_ae` 现在最多保留 16 个字节，并回退到最近的字符边界；`encode_ae` 再补齐到 16 字节。多字节标题回归测试验证不会产生破碎 UTF-8。

### 2.16 【已修复】响应命令不再写入 Message ID

`CommandField::writes_message_id` 只对请求（排除 C-CANCEL）返回 true，`Command::to_dataset` 因此仅在请求中写入 `(0000,0110)`；响应只带 Message ID Being Responded To。

### 2.17 【已修复】扩展协商字段越界会报告协议错误

扩展协商 UID、Common Extended 的 service/related SOP class UID，以及 User Identity
的两个 payload 长度现在都经过边界校验；越界统一返回 `NetworkError::Protocol`，不会
静默丢弃剩余字段。`a_common_extended_length_that_overruns_the_item_is_an_error`
覆盖 malformed item 的回归路径。

**正确行为**：与 `item_header` 对齐，返回 `NetworkError::protocol("app info field runs past the item")`。

---

### 2.18 【已修复】扩展协商答案不再清空无关字段

`extended::apply_answer` 现在只替换 SOP Class Extended Negotiation（0x56），保留 RQ-only 的 Common Extended Negotiation（0x57）。测试 `apply_answer_touches_only_the_extended_items` 固定了这一边界。

## 3. 性能问题

### 3.1 【已修复】`read_pdu` 单次分配、整帧读入

**位置**：`transport.rs:49-53`

```rust
let mut frame = Vec::with_capacity(HEADER_LEN + length as usize);  // 分配 1
frame.extend_from_slice(&header);
let mut body = vec![0u8; length as usize];                          // 分配 2（并零初始化）
reader.read_exact(&mut body).await.map_err(NetworkError::Io)?;
frame.extend_from_slice(&body);                                     // 整帧 memcpy
pdu::decode(&frame)
```

**问题**：每个 PDU 都要额外分配一个 `length` 大小的零初始化缓冲，再把它整体复制进 `frame`（然后立刻丢弃）。以 fo-dicom 默认 256 KiB PDU 计，每 PDU 白白多分配并触碰 256 KiB，外加一次全量 memcpy。对 C-STORE 的批量传输，这是每字节至少 2 次额外内存操作的常数级放大。

**最小修复**：先把头 6 字节读进 `frame`，再用 `frame.resize(HEADER_LEN + len, 0)` 就地扩容后直接 `read_exact(&mut frame[HEADER_LEN..])`。

```rust
let mut frame = vec![0u8; HEADER_LEN + length as usize];
frame[..HEADER_LEN].copy_from_slice(&header);
reader.read_exact(&mut frame[HEADER_LEN..]).await?;
```

一次分配，零拷贝。

### 3.2 【已修复】`write_pdu` 每 PDU 重新编码整帧

**现状**：`pdu::encode_into` 把 P-DATA 直接写入目标帧缓冲（不再先构造 body `Vec` 再拷贝）；一条消息的多个分片经 `write_pdus` 共用一个缓冲、一次 `write_all`。逐条控制 PDU 仍需一次输出分配（签名收 `&Pdu`），属必要开销。

**位置**：`transport.rs:59-64`

```rust
let bytes = pdu::encode(pdu);          // 每次分配并复制全部 PDV 数据
writer.write_all(&bytes).await?;
writer.flush().await?;                  // TcpStream 上 flush 是空操作
```

`pdu::encode`（`pdu.rs:563-578`）先算 body 再 `extend_from_slice`，而 `encode_pdata`（`pdu.rs:716-737`）对每个 PDV 又 `extend_from_slice(&pdv.data)`。但 `Pdv.data` 本身已经是从消息里 `to_vec()` 复制来的独立缓冲（见 3.3），所以一次发送至少要经历：原始消息 → `Pdv.data` 拷贝 → `encode` 输出拷贝 → socket。每 PDV 两次冗余全量拷贝。

**最小修复**：给 `write_pdu` 增加"已编码缓冲"路径（PDV 切片直接 `write_vectored`），或让 `Pdv` 持有 `Bytes` 借用而非 `Vec<u8>` 复制。`flush()` 对 `TcpStream` 无意义，可直接删掉。

### 3.3 【已修复】`Pdv.data` 改为 `Bytes`，发送分片零拷贝

**位置**：`message.rs:50-59` → `assembler.rs:123-159`（`data[offset..end].to_vec()`）、`pdu.rs:545`（`value.to_vec()`）、`pdu.rs:732`（`extend_from_slice`）

- `encode_command` / `encode_dataset` 各分配一个完整消息缓冲（`message.rs:18`、`message.rs:34`）；
- `split` 对每个分片 `data[offset..end].to_vec()`（`assembler.rs:155`），再复制一遍；
- `encode_pdata` 写出时第 3 次复制。

**最小修复**：`Pdv.data` 改为 `bytes::Bytes`（零拷贝切片），`split` 直接 `Bytes::slice`。

### 3.4 【已修复】解码 PDV 共享帧缓冲，单 PDV 消息不再拷贝

**位置**：`pdu.rs:545`

```rust
data: value.to_vec(),
```

整个 `body` 已经是 `frame` 的切片（`pdu.rs:275`），但每个 PDV 又整体复制一次。随后 `assembler` 还要把各 PDV 拼成完整消息（再复制）。大消息（如多帧影像）在接收侧至少 3 次全量拷贝：socket 缓冲 → `body` 零初始化+拷贝（见 3.1）→ 每 PDV `to_vec` → 组装缓冲。

**最小修复**：同上，`Bytes` 切片 + 组装时 `BytesMut::unsplit` 或直接保存 PDV 切片列表。

### 3.5 【低】`Vec` 无预分配上限，超大 `Vec::with_capacity` 由对端控制

**位置**：`pdu.rs:525`（`vec![]`）、`pdu.rs:529-546`

`decode_pdata` 对每个 PDV `push` 而不预留容量；PDV 数量由对端 PDU 长度决定（单 PDU 最多约 10922 个 6 字节 PDV），`Vec` 会多次 realloc。可 `Vec::with_capacity(body.len() / 8)` 作启发式预分配。

### 3.6 【低】`print_sop.rs` 每 PDU / 每次调用重建映射表

**位置**：`print_sop.rs:34-52`

```rust
pub fn instances() -> Vec<(&'static str, Vec<&'static str>)> { vec![...] }
pub fn sub_sop_classes(meta: &str) -> Option<Vec<&'static str>> {
    Self::instances().into_iter().find(...)     // 每次调用都新建两个 Vec
```

`instances()` 每次调用都构造新的嵌套 `Vec`，`sub_sop_classes` / `meta_sop_classes_for`（`print_sop.rs:57-68`）每次都重建整张表。应改为 `const` 数组或 `OnceLock`。

### 3.7 【已修复】`transport.rs` 的 `network_identifier` 与 print 侧语义统一

`transport::network_identifier()` 现在委托给 `print_sop::try_get_network_identifier(IMPLEMENTATION_CLASS_UID)`，
即 fo-dicom `NetworkManager.TryGetNetworkIdentifier` 的"实现类 UID + MAC（十进制）"形式，
不再使用与 `print_sop` 矛盾的"主机名 FNV 哈希 → `2.25.x`"。结果仍用 `OnceLock` 缓存，
每进程只计算一次。

---

## 4. 测试审查

本节保留首次走查时的测试缺口快照，便于追溯为什么这些问题曾经漏过；其中列出的
A-ASSOCIATE 线格、Priority、TLS 默认名、AbortSource、N-service、buffer、AE title、
Message ID 和 Common Extended 条目已经修复，当前实现与回归证据以 §2 和源码为准。

统计：`radicom-network` 共 248 个测试通过。以下仅针对在范围内的文件，按"是否会放过真实缺陷"分类。

### 4.1 False-green：断言了错误行为

| 测试 | 位置 | 问题 |
| --- | --- | --- |
| `priority_uses_the_standard_numeric_order`（历史） | `dimse.rs:554` | 曾断言错误的优先级顺序；当前实现和测试已改为 `Medium=0 / High=1 / Low=2`。 |
| `server_name_defaults_to_localhost`（历史） | `tls.rs:305` | 曾把 `localhost` 固化为默认证书名；当前默认值跟随 peer IP。 |
| `oversized_pdu_is_a_protocol_error` | `transport.rs:147` | 当前 65535 是明确的实现上限；还应配合测试通告值被 `clamp_max_pdu_length` 钳制，不能把该限制误写成 DICOM 的协议上限。 |
| `tcp_options_default_to_nodelay`（历史） | `transport.rs:167` | 曾只检查默认值而漏测配置生效；当前 socket option 回归直接读取内核值。 |
| `app_info_keeps_field_order_and_replaces_in_place`（历史） | `extended.rs:284` | 曾断言错误的可变长 `Vec` 模型；当前 `fields_are_flat_and_gaps_are_zero_filled` 固定扁平字段语义。 |
| `common_extended_is_rq_only`（历史） | `extended.rs:375` | 曾断言 `apply_answer` 清空 `common_extended`；当前实现保留 RQ-only 字段。 |
| `abort_codes_round_trip`（历史） | `error.rs:152` | 曾把保留 source 值 1 视为合法；当前解码拒绝该值。 |

### 4.2 【历史快照】同义反复：自编码→自解码，无法发现线格错误

这一组解释了原始 §2.1–§2.4 四个高危缺陷为什么曾经存活：测试只验证"radicom 能读懂 radicom"，对"是否与 PS3.8 一致"零覆盖。当前 `pdu.rs` 已补充标准字节断言。

| 测试 | 位置 | 被掩盖的缺陷 |
| --- | --- | --- |
| `associate_ac_carries_results_and_one_syntax` | `pdu.rs:802` | 历史：§2.1 只做自往返，现由 wire-layout fixture 补足 |
| `associate_ac_omits_abstract_syntax_and_carries_one_syntax` | `pdu.rs:884` | 历史：§2.1 同上 |
| `associate_rq_round_trips` | `pdu.rs:776` | 历史：§2.2/§2.3/§2.4 只断言字段回读 |
| `user_information_sub_items_round_trip` | `pdu.rs:922` | 历史：当前已有角色、扩展协商和 identity 字节测试 |
| `extended_negotiation_lands_on_the_side_matching_the_pdu` | `pdu.rs:964` | 历史：当前 flat application-info fixture 覆盖 |
| `user_information_carries_identity_and_roles` | `association.rs:679` | 历史：当前由 pdu wire fixture 补充 |
| `role_selections_survive_the_answer` | `extended.rs:400` | 历史：当前由 pdu wire fixture 补充 |
| `every_proposed_context_gets_an_answer` | `association.rs:550` | 历史：状态机测试仍保留，字节位置另有覆盖 |
| `context_state_codes_round_trip` | `association.rs:606` | 历史：当前 `Proposed -> None` 有专门断言 |
| `a_request_command_round_trips_through_its_dataset` | `nservice.rs:160` | 历史：当前 N-service tag 有 wire-level 断言 |
| `a_response_command_round_trips` | `nservice.rs:195` | 历史：当前响应 Message ID 规则有专门断言 |
| `network_identifier_is_a_valid_uid_shape` | `transport.rs:175` | 条件断言：`network_identifier()` 返回 `None` 时测试**什么都不检查**即通过；且不覆盖 `print_sop` 那条真正推荐的路径 |

### 4.3 覆盖良好、值得保留的测试

- `pdu.rs:823` `ae_titles_are_padded_and_trimmed` —— 唯一直查 AE title 线上字节的测试。
- `pdu.rs:840` `associate_item_values_are_even_length_on_the_wire` —— 递归校验所有 item 偶长度，思路正确（但见 §2.6 的长度值未校验）。
- `pdu.rs:1026` `pdata_odd_value_is_even_on_the_wire` —— 直查 PDV 长度与填充字节。
- `pdu.rs:908` `associate_rj_body_is_reserved_result_source_reason` —— 直查 RJ 4 字节布局。
- `pdu.rs:1074` `malformed_input_never_panics` / `pdu.rs:1093` `random_bytes_do_not_panic` —— 截断与随机字节的鲁棒性扫描。
- `pdu.rs:1035` `unknown_items_are_skipped_by_length` —— 手工构造含未知 item 的关联。
- `association.rs:432` `context_ids_exhaust_at_255` —— 边界正确。
- `tls.rs:256` `tls_handshake_fails_without_a_trusted_certificate` —— 负向 TLS 用例。
- `message.rs:136` `reassembly_of_a_split_message_is_lossless` —— 分片+重组端到端。

### 4.4 缺失的覆盖（按风险排序）

1. **A-ASSOCIATE 的字节级 golden test（已补齐）**：历史上这是唯一能捕获 §2.1–§2.4 的手段，因为自往返测试对它们**原理上无效**。当前 `pdu.rs` 已补充 AC result、role selection、extended negotiation 和 user identity 的标准字节断言。
   - AC：`pc_item[2] == result`（偏移 2，不是偏移 1）；
   - role：`item[0..2]` 是 UID 长度、UID 从偏移 2 开始、两个角色字节在 `2+len` 与 `3+len`；
   - ext neg：application information 长度 == 字段数（扁平），不是三元组；
   - user identity：`item[1]` 是 positive 标志，`item[2..4]` 是 primary 长度。
2. **Max-PDU 有界策略**：验证配置值 0 和 >65535 的通告值都被 `clamp_max_pdu_length` 限制到 65535，并保持 transport 对超上限 PDU 的拒绝；若要完全对齐 fo-dicom，再单独设计协商值驱动的读侧上限。
3. **N-Service tag 选择（已补齐）**：按 PS3.7 §10.1 表逐一断言四个服务用 `(0000,0003)/(0000,1001)`、两个服务用 `(0000,0002)/(0000,1000)`（当前 §2.11）。
4. **`apply_options` 的生效验证（已补齐）**：设置非 `None` 的 buffer size 后确认 `SO_SNDBUF` 实际改变（当前 §2.12）。
5. **`push_item` 超长路径（已补齐）**：构造单个 item > 65535 与整体 A-ASSOCIATE > 65535，断言返回错误（当前 §2.6）。
6. **AE title 非 ASCII（已补齐）**：中文/多字节输入下断言恰好 16 字节且不跨字符边界（当前 §2.15）。
7. **PDV 的 context id 合法性**：`decode_pdata` 不校验 `context_id` 为奇数且 1..=255，应加负向用例。
8. **分片边界的精确性**：`message.rs:118` `message_splitting_puts_the_command_first` 只断言 PDU 数量与位标志，未验证每个 PDU 的总长 ≤ max_pdu（含 6 字节头）。

### 4.5 一条方法论结论（含历史背景）

§2.1–§2.4 四类缺陷的共同特征是：**编码器与解码器共享同一个错误假设，因此自往返测试 100% 通过**。这是原始评审结论；当前实现已用标准字节测试覆盖。

由此得出的可推广结论：协议层的正确性不能由自往返测试保证，必须由**外部派生的 golden 字节**（标准条文或第三方实现的输出）保证。建议把 §4.4-1 作为协议层的准入条件——凡是新增/修改 A-ASSOCIATE 子项的改动，都必须同步更新 golden fixture。

---

## 附：建议修复顺序

1. **§2.1 / §2.2 / §2.3 / §2.4** —— 原始四处线格错误已修复，后续改动必须保留标准字节回归。
2. **§2.5** —— 当前仍是有界 Max-PDU 设计；完全对齐需要让读侧上限取协商值。
3. **§2.11** —— N-Service 全系列不可用的根因，改动局限在 `nservice.rs`。
4. **§2.7** —— 一行常量修正，影响所有 DIMSE 请求的优先级语义。
5. **§2.12 / §2.13 / §2.14** —— 部署可信度与安全性。
6. §3.1 / §3.2 —— 吞吐优化，改动局部且可独立验证。
7. 其余中/低项按需。

> 关于 §2.1–§2.5 的修复验证方式：不要用"改完自往返通过"作为完成标准——那正是当初漏掉线格问题的原因。当前四类线格已有标准字节 fixture；Max-PDU 则按有界策略验证。

---

*评审范围限于协议层文件，未覆盖 `server.rs`、`client.rs`、`engine.rs`、`cfind.rs`、`cget.rs`、`cmove.rs`、`store.rs`、`services.rs`、`hosting.rs`、`assembler.rs`、`expert.rs`、`observability.rs`（由另一任务负责）。本文对 `assembler.rs` / `server.rs` 的引用仅作为调用点可达性证据。*
