# 网络协议层评审（`radicom-network` 协议层）

评审对象：`crates/radicom-network/src/` 下协议层文件 —— `pdu.rs`、`transport.rs`、`tls.rs`、`association.rs`、`dimse.rs`、`message.rs`、`extended.rs`、`nservice.rs`、`print_sop.rs`、`error.rs`、`lib.rs`。

参照实现：fo-dicom `FO-DICOM.Core/Network/`（`PDU.cs`、`DicomService.cs`、`DicomStatus.cs`、`DicomPresentationContext.cs`、`DicomMessage.cs`、`DicomServiceApplicationInfo.cs`、`DicomUserIdentityNegotiation.cs`、`Tls/*.cs` 等），以及 DICOM PS3.8（Upper Layer Protocol）、PS3.7（DIMSE）、PS3.4（Service Class）。

方法：逐行阅读上述 Rust 源码，与 fo-dicom C# 的写/读两侧逐字段比对；只在读完实际代码并确认调用点可达后给出结论。repo 内 `docs/*.md` 的自评不作为依据。本文只做评审，未修改任何代码。

### 证据强度

本报告对每条结论标注证据强度：

| 标注 | 含义 |
| --- | --- |
| **字节验证** | 已用按 PS3.8/PS3.7 手工构造的标准字节喂给 `pdu::decode`，以及取 `pdu::encode` 的输出做逐字节比对，真实复现了错误行为。强证据。 |
| **源码比对** | 逐字段比对 radicom 与 fo-dicom 两侧读写代码，未构造字节。结论正确性依赖比对本身。 |

按标准生成字节的生成器与探针放在系统临时目录（未进入仓库）：`<tmp>/oracle/mkconformant.py` 生成 fixture，`<tmp>/oracle/confcheck/` 验证解码，`<tmp>/oracle/confform/` 验证编码。§2.1–§2.5 各条附了关键字节与复现输出，可直接用作回归测试的 golden 数据。

**基线**：workspace 构建通过、`clippy` 无告警、`radicom-network` 248 个测试通过（由 Lead 统一验证，本轮未重复全量验证）。

**总体结论**：传输/分帧骨架、DIMSE 命令字编码、错误类型设计质量较好；但 A-ASSOCIATE 的**四个用户信息/表现上下文子项的线格布局全部与 PS3.8/PS3.7 不一致**，且这些偏差被"自己编码、自己解码"的往返测试完全掩盖——编码器与解码器共享同一个错误假设，所以自往返永远通过。这四项均已用标准字节验证，属于会直接导致与 fo-dicom / DCMTK 无法互通的**高危缺陷**。

**重要的共性**：这四类 bug 无法被任何"自往返"测试发现，只能靠外部互通测试或标准派生 golden 字节捕获。因此 §4.4 建议的 golden fixture 不是锦上添花，而是这四项修复后唯一能防回归的手段。

本次评审共发现：

| 级别 | 数量 | 证据强度 | 说明 |
| --- | --- | --- | --- |
| 高 | 6 | §2.1–§2.4 字节验证；§2.5 字节验证（通告值往返 + 强制上限矛盾） | 线格不兼容或造成错误语义，对接第三方必然出问题 |
| 中 | 8 | 源码比对 | 边界/健壮性/规范偏差 |
| 低 | 7 | 源码比对 | 命名、效率、可维护性 |

---

## 1. 功能缺失（对比 fo-dicom，仅协议层）

状态取值：**已实现** / **部分实现** / **缺失** / **有意不实现**。"缺失"仅在已 grep 确认 repo 内无对应实现后标注。

### 1.1 PDU 编解码

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 7 种 PDU 类型 | `PDU.cs:RawPduType` | 已实现 | `pdu.rs:262` `decode` / `pdu.rs:563` `encode` | 0x01–0x07 全覆盖 |
| 6 字节 PDU 头（type/reserved/len BE） | `PDU.cs:RawPDU.CommonFieldsLength` | 已实现 | `transport.rs:12` `HEADER_LEN`、`transport.rs:35` `read_pdu` | 读 1 字节再读 5 字节，可正确处理 EOF |
| A-ASSOCIATE-RQ 编解码 | `PDU.cs:547` | 部分实现 | `pdu.rs:615` `encode_associate`、`pdu.rs:319` `decode_associate` | 用户信息子项布局有误（0x54/0x56/0x58 三项均已字节验证），见 §2.2–§2.4 |
| A-ASSOCIATE-AC 编解码 | `PDU.cs:915` | 部分实现 | 同上（`is_rq=false`） | **presentation context result 字节位置错误（已字节验证）**，见 §2.1 |
| A-ASSOCIATE-RJ 编解码 | `PDU.cs:AAssociateRJ` | 已实现 | `pdu.rs:514` `decode_associate_rj`、`pdu.rs:567` | 4 字节体，符合 §9.3.4 |
| P-DATA-TF 编解码 | `PDU.cs:1569` `PDataTF` | 已实现 | `pdu.rs:524` `decode_pdata`、`pdu.rs:716` `encode_pdata` | PDV 位定义、偶长填充均正确 |
| A-RELEASE-RQ/RP | `PDU.cs` | 已实现 | `pdu.rs:569` | — |
| A-ABORT | `PDU.cs` | 已实现 | `pdu.rs:552` `decode_abort`、`pdu.rs:570` | source 语义见 §2.9 |
| 16 位 item 长度回填 | `PDU.cs` `MarkLength16` / `WriteLength16` + `Stack<long>` | 有意不实现 | `pdu.rs:585` `push_item` | Rust 侧直接算好长度后整体写出，无需回填；但**缺少 u16 溢出保护**，见 §2.6 |

### 1.2 用户信息子项（User Information，item 0x50）

| 子项 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 0x51 Max-PDU-Length | `PDU.cs:596-600` | 部分实现 | `pdu.rs:396` 解码 / `pdu.rs:648` 编码 | 值解析正确，但硬上限 65535 与之矛盾，见 §2.5 |
| 0x52 Implementation Class UID | `PDU.cs:602-607` | 已实现 | `pdu.rs:401` / `pdu.rs:649` | — |
| 0x53 Asynchronous Operations Window | `PDU.cs:609-617`（仅在非 (1,1) 时发送） | 部分实现 | `pdu.rs:407` / `pdu.rs:655-662` | 编解码正确、默认省略也正确；但**协商结果从不被引擎使用**，无并发窗口控制 |
| 0x54 SCP/SCU Role Selection | `PDU.cs:619-634` | **部分实现（线格错误，已字节验证）** | `pdu.rs:415-423` 解码 / `pdu.rs:663-670` 编码 | 字段顺序整体颠倒，UID 被静默平移，见 §2.2 |
| 0x55 Implementation Version Name | `PDU.cs:636-641` | 已实现 | `pdu.rs:404` / `pdu.rs:652` | — |
| 0x56 SOP Class Extended Negotiation | `PDU.cs:643-654` | **部分实现（线格错误，已字节验证）** | `pdu.rs:424` / `pdu.rs:671-682` | application information 格式错误，字段被静默丢弃，见 §2.3 |
| 0x57 SOP Class Common Extended Negotiation | `PDU.cs:656-677` | 已实现 | `pdu.rs:432` / `pdu.rs:683-694` | 仅 RQ，符合 §9.3.2.3 |
| 0x58 User Identity Negotiation (RQ) | `PDU.cs:679-694` | **部分实现（线格错误，已字节验证）** | `pdu.rs:438-451` / `pdu.rs:695-706` | 字段顺序错误，三个 payload 字段全部丢失，见 §2.4 |
| 0x59 User Identity Negotiation (AC) | `PDU.cs:1017-1024` | 部分实现 | `pdu.rs:452` / `pdu.rs:707-711` | 应答体是不透明 `Vec<u8>`，无长度/类型校验 |
| 未知子项按长度跳过 | `PDU.cs:1164` `SkipBytes` | 已实现 | `pdu.rs:455`、`pdu.rs:345` | — |

### 1.3 关联协商与策略

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| presentation context ID 奇数分配 1..255 | `DicomPresentationContextCollection.cs:GetNextPresentationContextID` | 已实现 | `association.rs:89` `ContextIdAllocator` | 255 后正确耗尽 |
| 重复 context 去重 | `DicomPresentationContextCollection`（按 abstract+roles+TS 集合） | 已实现 | `association.rs:125` `is_duplicate_of` | TS 集合排序后比较，角色按 SOP class 排序比较，语义正确 |
| 抽象语法优先于传输语法判定 | `DicomService` 协商逻辑 | 已实现 | `association.rs:292` `negotiate_one` | 不支持抽象语法 → 3（RejectAbstractSyntaxNotSupported） |
| 传输语法接受顺序（Offered / Preferred） | fo-dicom 无同名开关，按 propose 顺序取首个匹配 | 已实现（超出参照） | `association.rs:304` `SyntaxPriority` | 提供两种策略，默认应与 fo-dicom 对齐（取 offered 顺序） |
| 结果码 0–4 | `DicomPresentationContext.cs:DicomPresentationContextResult` | 部分实现 | `association.rs:49` `result_code` | `Proposed → 0` 与 fo-dicom `Proposed = 255` 不等价，见 §2.10 |
| 角色选择授予策略 | `PDU.cs` + `DicomPresentationContext.UserRole/ProviderRole` | 已实现 | `association.rs:576` 测试覆盖、`association.rs:341` | 策略正确，但写上线时线格错误（§2.2） |
| 已接受 context 的路由选择 | `DicomService` `_assoc.PresentationContexts.FirstOrDefault` | 已实现 | `association.rs:357` `AcceptedContexts::pick` | 精确 TS 优先，回退首个（`association.rs:622-663` 有覆盖） |

### 1.4 DIMSE 命令集

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 22 个 Command Field | `DicomCommandField.cs` | 已实现 | `dimse.rs:104` `code` / `dimse.rs:136` `from_code` | 逐个核对，与 fo-dicom 完全一致 |
| 请求/响应 bit15 判定 | `DicomMessage.cs:IsRequest` → `((int)type & 0x8000) == 0` | 已实现 | `dimse.rs:160` 附近 `is_request` | — |
| 命令集恒为 Implicit VR LE | `DicomService` 命令编码路径 | 已实现 | `message.rs:16` `encode_command` | 显式固定 `IMPLICIT_VR_LITTLE_ENDIAN` |
| (0000,0000) 组长度重算 | `DicomMessage` 编码 | 已实现 | `dimse.rs:300-306` | 用 writer 算长度再减 12，有测试 `dimse.rs:580` |
| (0000,0800) Command Data Set Type | `DicomMessage.cs:127`（`0x0202` / `0x0101`） | 已实现 | `dimse.rs:44` `NO_DATA_SET`、`dimse.rs:257-262` | 用 `0x0001` 表示"有数据集"，PS3.7 允许（≠0101H 即可），但与 fo-dicom 的 `0x0202` 不同 |
| 优先级元素 | `DicomPriority.cs` | **部分实现（取值错误）** | `dimse.rs:49` `Priority` | 数值映射与默认值均错误，见 §2.7 |
| 状态码分类 | `DicomStatus.cs:Lookup`（mask + 通配 `Bxxx`/`Cxxx`/`A7xx`） | 部分实现 | `dimse.rs:444` `classify` / `dimse.rs:470` `exact_state` | 有按类归并，但**无通配字符串表、无 `AddKnownDicomStatuses` 式可扩展注册** |
| 状态码常量表 | `DicomStatus.cs:311-515` | 部分实现 | `dimse.rs:483-508` `status` | 常量被别名合并，语义丢失，见 §2.8 |
| 状态码 → 名称/描述 | `DicomStatus.Description`、`DicomStatus.ToString` | 缺失 | — | grep 确认无任何 code→文本映射 |
| 离线命令集解析容错 | `DicomMessage` 从 Dataset 构造 | 已实现 | `dimse.rs:311` `from_dataset` | 缺字段用 `get_or` 兜底 |

### 1.5 N-Service（DIMSE-N）

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 6 个 N 请求/响应字段 | `DicomCommandField.cs` | 已实现 | `nservice.rs:18` `N_REQUEST_FIELDS` | — |
| N-CREATE / N-EVENT-REPORT 用 Affected* | `DicomMessage.cs:74-76`、`DicomNCreateRequest.cs:45` | 已实现 | `nservice.rs:57-58` | — |
| N-GET / N-SET / N-ACTION / N-DELETE 用 Requested* | `DicomMessage.cs:64-68`、`DicomNGetRequest.cs:45`、`DicomNSetRequest.cs:24`、`DicomNActionRequest.cs:51`、`DicomNDeleteRequest.cs:24` | **缺失** | `nservice.rs:33-34`、`nservice.rs:57-58`、`nservice.rs:124-125` | 六个服务全部只用 `affected_*`，`requested_*` 从不赋值，见 §2.11 |
| N-GET 的 Attribute Identifier List (0000,1005) | `DicomNGetRequest.cs` + `DicomTag.AttributeIdentifierList` | **缺失** | `nservice.rs:30-38` | 结构体无该字段；`dimse.rs:34` 只定义了 tag 常量，全 crate 无使用者 |
| N-SET 的 Modification List | PS3.7 §10.1.2 | **缺失** | — | grep 确认无实现（注意：ModificationList 实际走 Dataset，不强制） |
| N 响应回显 Affected SOP Instance UID | `DicomNGetResponse.cs` 等 | 已实现 | `nservice.rs:110-111`、`nservice.rs:124-125` | — |
| N 响应回显 Event/Action Type ID | `DicomNEventReportResponse.cs`、`DicomNActionResponse.cs` | 已实现 | `nservice.rs:112-113`、`nservice.rs:126-127` | — |
| N-CREATE-RSP 返回新建实例 UID | `DicomNCreateResponse.cs` | 部分实现 | `nservice.rs:105-117` `from_request` | 仅复制请求侧值，无法携带服务端新建的实例 UID |

### 1.6 传输 / TLS / 平台

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| TCP_NODELAY | `NetworkStreamCreationOptions` | 已实现 | `transport.rs:68` `apply_options` | 默认开启 |
| SO_SNDBUF / SO_RCVBUF | `NetworkStreamCreationOptions.Send/ReceiveBufferSize` | **缺失（字段存在但被丢弃）** | `transport.rs:20-22`、`transport.rs:70-71` | 两个字段被 `let _ = ...` 直接丢弃，见 §2.12 |
| TLS 服务端 / 客户端握手 | `Tls/DefaultTlsAcceptor.cs`、`Tls/DefaultTlsInitiator.cs` | 已实现 | `tls.rs:89` `accept` / `tls.rs:96` `connect` | 基于 tokio-rustls |
| 显式最低协议版本 | `SslProtocols.Tls12 \| Tls13`（fo-dicom 在 `SslClientAuthenticationOptions` 显式指定） | 部分实现 | `tls.rs:54-57` | 未显式声明，靠 rustls 默认下限（当前为 TLS1.2）；见 §2.13 |
| 双向认证（mTLS） | `DefaultTlsAcceptor.RequireMutualAuthentication` | **缺失** | `tls.rs:54` `with_no_client_auth()` | 服务端硬编码不要求客户端证书 |
| 证书校验开关 | `DefaultTlsInitiator.IgnoreSslPolicyErrors` | 已实现 | `tls.rs:71-79` `accept_any` | 语义等价，注释已标明 "testing only" |
| SNI / 证书名指定 | `TargetHost` | 部分实现 | `tls.rs:84-86` `server_name` | 默认回落到字符串 `"localhost"`，见 §2.14 |
| 证书格式支持 | PKCS#12 + 密码 / Windows 证书库 | 部分实现 | `tls.rs:114` `load_certs`、`tls.rs:125` `load_key` | 仅 PEM（PKCS#8 / PKCS#1）；PKCS#12 与系统证书库缺失 |
| 三类超时（ACSE / DIMSE / Socket） | `DicomTimeout.cs` | **缺失** | `error.rs:102-105` | 只有 `AssociationRequestTimedOut` / `RequestTimedOut` 两种错误变体，无 Socket 级超时概念，无 `DicomTimeout` 三态枚举 |
| 平台抽象层 `INetworkManager` | `NetworkManager.cs`（Listener/Stream/IsSocketException/MachineName/TryGetNetworkIdentifier） | **缺失** | `transport.rs:79`、`print_sop.rs:91` | 无统一抽象；机器标识在两处各写了一份（`transport.rs:79` 按主机名 FNV 哈希，`print_sop.rs:76` 按 MAC 拼接），语义互相矛盾 |
| 机器标识 | `NetworkManager.TryGetNetworkIdentifier`（MAC 追加到实现类 UID） | 部分实现 | `print_sop.rs:76` `network_identifier_from_mac` | 该实现与 fo-dicom 对齐；`transport.rs:79` 的 `2.25.{fnv}` 是另一套语义 |
| 命令/数据缓冲上限 | `DicomServiceOptions.MaxCommandBuffer` / `MaxDataBuffer` | 部分实现 | `assembler.rs:108` `PduSplitter::new` | 只有单一 `max_pdu` + `max_pdvs_per_pdu`，未区分命令与数据缓冲 |
| 每 PDU PDV 数上限 | `DicomServiceOptions.MaxPDVsPerPDU` | 已实现 | `assembler.rs:108` | 参数形态一致 |

### 1.7 打印 / 其它

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| Meta SOP Class → 子类映射 | `DicomService.cs:1241` `MetaSopClasses.GetMetaSopClass` | 已实现 | `print_sop.rs:34` `instances` | 覆盖 Basic Grayscale / Color 两个 meta 类，与 fo-dicom 一致 |
| 反向查找（含 DefaultIfEmpty） | `MetaSopClasses.GetMetaSopClass` | 已实现 | `print_sop.rs:57` `meta_sop_classes_for` | 未命中时返回自身，语义对齐 |
| MAC → DICOM UID | `NetworkManager.TryGetNetworkIdentifier` | 已实现 | `print_sop.rs:76` | 十进制追加，合法 |
| Print SOP UID 常量 | `DicomUID` | 已实现 | `print_sop.rs:10` `uids` | — |

---

## 2. 正确性缺陷

### 2.1 【高｜字节验证】A-ASSOCIATE-AC 的 presentation context result 字节位置错误

**位置**：`pdu.rs:365-366`（解码）、`pdu.rs:626-629`（编码）

```rust
// pdu.rs:365-366 解码
let id = value[0];
let result = value[1];        // ← 应为 value[2]

// pdu.rs:626-629 编码
body.push(pc.id);
body.push(if is_rq { 0 } else { pc.result });   // ← 结果写到了偏移 1
body.extend_from_slice(&[0, 0]);                // ← 偏移 2/3 恒为 0
```

**为什么错**：PS3.8 §9.3.3.2 规定 A-ASSOCIATE-AC 的 Presentation Context Item 体为 `ID(1) + Reserved(1) + Result/Reason(1) + Reserved(1)`。fo-dicom 写侧（`PDU.cs:936-939`）与读侧（`PDU.cs:1073-1076`）都是 `id, reserved, result, reserved`；radicom 把 result 放在偏移 1、把 reserved 放在偏移 2，两者互换。

注意编码侧有**两处**相互独立的问题，虽然长度都正确（4 字节）：
- 位置错：`pc.result` 写到了偏移 1；
- 偏移 2 被无条件写成 `0x00`。

二者叠加使"偏移 2 永远是 0"，这正是下面方向 2 后果特别严重的原因。

**实际后果（双向，均为高危）**：
1. **收方向**：收到 fo-dicom / DCMTK 的 AC 时，radicom 读 `value[1]`（对端的 reserved，恒为 0）当 result，于是**所有** presentation context 都被判为 Accept（`result == 0`），包括对端明确拒绝的。`association.rs:229` `is_accepted()` 随之返回 true，引擎会在对端已拒绝的 context 上发送数据。
2. **发方向（更严重）**：radicom 写出的 AC 中偏移 1 = result、偏移 2 = 0。**任何合规对端读偏移 2，得到的永远是 0 = Accept**。所以 radicom 打算拒绝的 context，在对端眼里全部是"已接受"——拒绝被静默降级为接受，且 radicom 侧毫无感知。

**复现证据（字节验证）**：

按 PS3.8 §9.3.3 手工构造 176 字节 AC：context 1 = ACCEPT(0)，context 3 = REJECT(3)，喂给 `pdu::decode`：

```
context id=1 result=0 accepted=true
context id=3 result=0 accepted=true      <-- 错，标准字节在偏移 2 写的是 3
```

再用 `pdu::encode` 重新编码并 dump presentation context item 体：

```
id=1 bytes=[01, 00, 00, 00]
id=3 bytes=[03, 00, 00, 00]     <-- 偏移 1 = result 3，偏移 2 = 0
```

对同一结构单独验证发方向（context 3，`pc.result = 3`）：

```
#1 AC ctx id=3 -> item[1]=0x03 item[2]=0x00
   合规对端读 item[2]=0x00 作为 Result -> ACCEPT（错，本意是 REJECT 3）
```

golden fixture（176 字节，可直接做回归测试）：

```
0200000000AA0001000043414C4C45442020202020202020202043414C4C494E47202020202020202020000000000000000000000000000000000000000000000000000000000000000010000016312E322E3834302E31303030382E332E312E312E31002100001C0100000040000014312E322E3834302E31303030382E312E322E31002100001C0300030040000014312E322E3834302E31303030382E312E322E3100500000085100000400020000
```

**正确行为**：编解码统一为 `id, reserved, result, reserved`（4 字节，偏移 0/1/2/3）。RQ 方向 radicom 现状 `id,0,0,0` 恰好符合该布局，所以**一处统一的布局定义即可同时修好两个方向**，不需要分支。

**为什么测试没抓到**：`pdu.rs:802` `associate_ac_carries_results_and_one_syntax` 与 `pdu.rs:884` `associate_ac_omits_abstract_syntax_and_carries_one_syntax` 都走 `round_trip()`（编码→解码），共享同一个错误假设，必然一致。没有任何测试断言 AC 的 result 在**字节偏移 2**。

---

### 2.2 【高｜字节验证】Role Selection 子项（0x54）线格与 PS3.8 相反

**位置**：`pdu.rs:663-670`（编码）、`pdu.rs:415-423`（解码）

```rust
// 编码 pdu.rs:664-669
let mut value = Vec::with_capacity(4 + role.sop_class.len());
value.extend_from_slice(&[0, 0]);          // 注释写 "reserved u16"，实际占的应是 UID 长度
value.push(u8::from(role.scu_role));
value.push(u8::from(role.scp_role));
value.extend_from_slice(role.sop_class.as_bytes());   // UID 放到最后

// 解码 pdu.rs:417-421
sop_class: decode_string(&raw[4..]),      // 跳过前 4 字节再取 UID
scu_role: raw[2] != 0,
scp_role: raw[3] != 0,
```

**为什么错**：PS3.8 §9.3.2.3 / D.3.3.4 规定该子项体为 `SOP-class-UID-length(2, BE) + SOP-class-UID + SCU-role(1) + SCP-role(1)`。UID 长度在开头，角色字节在**尾部**。fo-dicom 写侧（`PDU.cs:626-631`）与读侧（`PDU.cs:1127-1130`）都是 `MarkLength16(UID-Length) + UID + SCU + SCP`。

radicom 把开头 2 字节写成 `0x00 0x00`，角色紧随其后，UID 放在最后——整个子项体的字段顺序完全颠倒。

**实际后果**：失败模式比"UID 长度读成 0"更隐蔽——**UID 被静默平移后仍然"解析成功"**，且角色值读自错误的字节。

**复现证据（字节验证）**。按 PS3.8 D.3.3.4 构造标准子项：`uid_len(2 BE) + UID + scu(1) + scp(1)`，UID = `1.2.840.10008.5.1.4.1.1.7`，`scu=1, scp=0`，喂给 `pdu::decode`：

```
ROLE sop="2.840.10008.5.1.4.1.1.7\u{1}" scu=true scp=true
```

三点观察：
- **UID 被平移 2 字节**：丢了开头的 `1.`，尾部多了 `\u{1}`（把 SCP 角色字节当作 UID 文本的一部分）；
- **`scp` 读错**：我发送的是 `scp=0`，radicom 报 `scp=true`；
- **解析"成功"**：没有报错，调用方完全无法察觉。

这个平移后的 UID 不会匹配任何已注册的 SOP class，所以角色协商失败是**静默的**——`association.rs:341` 的 `can_scu` 找不到匹配，反向 C-STORE 请求直接落空。

radicom 自己编码的字节（`confform` 输出，`scu=1, scp=0`）：

```
54 00 00 1E                                  item 0x54, length 30
00 00                                        <-- PS3.8 此处应为 UID 长度 0x001A
01 00                                        <-- 角色字节，对端会当成 UID 的前两个字节
31 2E 32 2E ... 2E 37 00                     UID 在最后，对端按 uid_len=0 读到空串
```

即对端解析出的 UID 长度为 0、UID 为空串，角色字节取自它以为的 UID 区。

**正确行为**：编码 `[uid_len_be16][uid][scu][scp]`；解码先 `u16_at(raw, 0)` 得 UID 长度 L，UID 取 `raw[2..2+L]`，SCU 取 `raw[2+L]`，SCP 取 `raw[3+L]`（子项值的偶长填充落在角色字节之后，不影响偏移）。

**为什么测试没抓到**：`pdu.rs:922` `user_information_sub_items_round_trip`、`association.rs:679` `user_information_carries_identity_and_roles`、`extended.rs:400` `role_selections_survive_the_answer` 全部是自编码自解码或纯内存断言。实测自往返输出：

```
#2 self round-trip: sop="1.2.840.10008.5.1.4.1.1.7" scu=true scp=false
```

字段全对——因为编码器与解码器共享同一个错误布局。无一处校验 0x54 的字节顺序。

---

### 2.3 【高｜字节验证】SOP Class Extended Negotiation（0x56）的 application information 格式错误

**位置**：`pdu.rs:671-682`（编码）、`pdu.rs:477-492` `decode_app_info`（解码）

```rust
// 编码 pdu.rs:676-680：每个字段写成 field(1) + length(2) + value
for (field, bytes) in info {
    value.push(*field);
    value.extend_from_slice(&(bytes.len() as u16).to_be_bytes());
    value.extend_from_slice(bytes);
}
```

**为什么错**：PS3.7 §D.3.3.5 与 fo-dicom 实现中，SOP Class Extended Negotiation 子项的 Service Class Application Information 是一段**扁平字节数组**，下标即字段号（1-based），每个字段恰好 1 字节，**没有逐字段长度前缀**。证据：`DicomServiceApplicationInfo.cs:36-45` 构造函数 `for (byte i = 1; i <= rawApplicationInfo.Length; i++) _fields.Add(i, rawApplicationInfo[i - 1])`；`GetValues()` 返回 `_fields.Values.ToArray()`（`DicomServiceApplicationInfo.cs:215`）；`PDU.cs:652` / `PDU.cs:1013` 直接 write 该数组。

radicom 写的是 `(field, len, value)` 三元组流。以 Storage 的 3 个字段为例：radicom 发 12 字节，fo-dicom 只发 3 字节。对端会把 radicom 的第一个字段号当"第 1 个字段的值"、把长度高字节当"第 2 个字段的值"…… 全部错位。

**实际后果**：整个扩展协商**被静默丢弃**——SOP class 还在，字段全空。

**复现证据（字节验证）**。按 PS3.7 构造标准 0x56 子项：`uid_len(2) + UID + [00,01,04]`（扁平 3 字节），喂给 `pdu::decode`：

```
EXT sop="1.2.840.10008.5.1.4.1.1.7" fields=[]
```

SOP class 正确解析，但 application information 为空——因为扁平字节 `[00,01,04]` 永远无法构成一个合法的 `(field, len, value)` 三元组（解码器要求至少 3 字节且长度自洽）。

radicom 自己编码的字节（`confform` 输出，字段 1/2/3 = 1/1/0）：

```
56 00 00 28                                  item 0x56, length 40
00 19                                        uid_len = 25
31 2E 32 2E ... 2E 37                        UID
01 00 01 01                                  <-- 三元组：(field=1, len=1, value=1)
02 00 01 01                                  <--           (field=2, len=1, value=1)
03 00 01 00                                  <--           (field=3, len=1, value=0)
```

合规对端读这 12 字节为扁平数组，得到 `field1=01, field2=00, field3=01, field4=01, field5=02, ...` —— 6 个字段全是垃圾值，而真实只有 3 个字段。

**正确行为**：编解码都应是扁平 `Vec<u8>`，第 i 个字节 = 字段 i 的值；写入时按字段号升序补齐空洞（fo-dicom `FillInTheGaps`，`DicomServiceApplicationInfo.cs:139`）。

**为什么测试没抓到**：`pdu.rs:964` `extended_negotiation_lands_on_the_side_matching_the_pdu` 只用 `round_trip`；`extended.rs:284` `app_info_keeps_field_order_and_replaces_in_place` 甚至把"按插入序、允许变长"这个错误模型当成规格断言下来：

```rust
let info = AppInfo::new().with_field(3, vec![1]).with_field(1, vec![2]).with_field(3, vec![9]);
assert_eq!(info.fields, vec![(3, vec![9]), (1, vec![2])], "order of first insertion, last value wins");
```

> 附带缺陷：`extended.rs:36-39` 用 `Vec<(u8, Vec<u8>)>` 保存字段，保留插入顺序且允许多字节值；fo-dicom 用 `SortedDictionary<byte, byte>` + 空洞填充。即使线格修正，内存模型仍需改为定长/有序语义。

---

### 2.4 【高｜字节验证】User Identity Negotiation（0x58）字段顺序错误

**位置**：`pdu.rs:695-705`（编码）、`pdu.rs:438-451`（解码）

```rust
// 编码 pdu.rs:697-703
value.push(identity.identity_type);
value.extend_from_slice(&(identity.primary.len() as u16).to_be_bytes());
value.extend_from_slice(&(identity.secondary.len() as u16).to_be_bytes());  // ← 两个长度都放前面
value.extend_from_slice(&identity.primary);
value.extend_from_slice(&identity.secondary);
value.push(u8::from(identity.positive_response_requested));                 // ← 标志放在最后
```

**为什么错**：PS3.8 §D.3.3.7 规定顺序为
`User-identity-type(1) + Positive-response-requested(1) + Primary-field-length(2) + Primary-field + Secondary-field-length(2) + Secondary-field`。

fo-dicom 读侧逐字段印证（`PDU.cs:837-842`）：`ReadByte(type)`、`ReadByte(positiveResponseRequested)`、`ReadUInt16(primaryLen)`、`ReadString(primary)`、`ReadUInt16(secondaryLen)`、`ReadString(secondary)`。fo-dicom 写侧（`PDU.cs:685-692`）顺序一致。

radicom 把 `positive_response_requested` 挪到末尾，并让两个长度字段相邻。

**实际后果**：**三个 payload 字段全部丢失**（不只是错位）。

**复现证据（字节验证）**。按 PS3.8 D.3.3.7 构造标准子项：`type=2, positive=1, primary="USER1", secondary="PASS1"`，喂给 `pdu::decode`：

```
IDENT type=2 primary="" secondary="" pos=false
```

type 正确（它是第 1 字节，恰好没被移动），但 primary、secondary、positive 三者全空/全假——解码器按自己的（错误）顺序去读，把标准字节的 `positive` 字节当作 primary 长度高字节，随后偏移全错，`raw.get(...)` 系列调用全部落到越界范围并静默返回 `unwrap_or_default()`（空 `Vec`）。

radicom 自己编码的字节（`confform` 输出，`type=2, primary="alice", secondary="secret", pos=1`）：

```
58 00 00 12                     item 0x58, length 18
02                              type = 2
00 05                           <-- 本应是 positive(1字节)+primary_len(2字节)，radicom 在此放 primary 长度高字节
00 06                           <-- 本应是 primary 的首字节，radicom 在此放 secondary 长度
61 6C 69 63 65                  "alice"（本应紧跟自己的长度字段之后）
73 65 63 72 65 74               "secret"
01                              <-- positive 标志，被挪到最末尾
00                              偶长填充
```

合规对端读这段：`type=2`，`positive = 0x00`（错，本意 1），`primary_len = 0x0005`，`primary = "alice"` 恰好凑对…… 但 `secondary_len` 从 `"secret"` 区读取，长度与内容错位，最终 secondary 变成垃圾或越界。**这不是"字段错位但仍可用"，而是对端会解析出错误的口令长度**，认证必然失败且可能读越界。

**正确行为**：`type(1) + positive(1) + primary_len(2) + primary + secondary_len(2) + secondary`。

**为什么测试没抓到**：`pdu.rs:939` 构造 `UserIdentity` 后只做 `round_trip` 并断言各字段回读相等；`pdu.rs:1107` `user_identity_validation_rules` 只测 `validate()` 的类型/口令约束，不碰线格。自往返之所以通过，是因为编码器与解码器对"两个长度在前、标志在后"这一错误布局达成了一致。

---

### 2.5 【高｜字节验证：通告值往返 + 强制上限矛盾】`MAX_PDU_SIZE = 65535` 硬上限使 >64 KiB 的 max-PDU 协商必然失败

**位置**：`pdu.rs:10` `MAX_PDU_SIZE`、`pdu.rs:268`、`transport.rs:46-48`

```rust
pub const MAX_PDU_SIZE: u32 = 65535;
// transport.rs:46
if length > MAX_PDU_SIZE {
    return Err(NetworkError::protocol("PDU length exceeds 65535"));
}
```

**为什么错**：65535 是 **A-ASSOCIATE item length 为 u16** 带来的限制，只约束 A-ASSOCIATE 本身。P-DATA-TF 的 PDU 长度字段是 u32（PS3.8 §9.3.5），其上限由协商出的 Max-PDU-Length 决定。fo-dicom 的默认值是 262144（`DicomServiceOptions.cs:74`：`public uint MaxPDULength { get; set; } = 262144; // 256 Kb`），且 max-PDU = 0 表示不限。

radicom 的 `engine.rs:30` 与 `server.rs:85` 把默认 `max_pdu_length` 设为 16384 并在 RQ 中通告该值，同时该值是可配置的 `u32`。一旦用户把它设为 131072：
1. radicom 向对端通告 Max-PDU-Length = 131072；
2. 对端据此合法地发送长度 131072 的 P-DATA-TF；
3. radicom 的 `transport.rs:46` 直接以 Protocol 错误断链。

即"自报能力后拒绝履行"。

**复现证据（字节验证）**：通告值本身能被正确解码，这让矛盾特别容易漏掉。

- 用含 `max_pdu = 131072` 的标准 AC 喂 `pdu::decode`，`max_pdu_length=131072` 正确读出（大端解析无误）；
- 再构造一个长度 131072 的 P-DATA-TF 帧（这正是对端履行该通告时会发出的帧）：

```
#5 P-DATA-TF of 131072 bytes (advertised max_pdu honored): REJECTED
   -> Protocol("PDU length exceeds 65535")
```

所以缺陷不在解析，而在"通告的能力"与"强制的上限"互相矛盾：值被如实转发出去，履行时却被自己的常量拒绝。

**正确行为**：读侧上限应取 `min(自报的 max_pdu_length, 某个实现上限)`，而不是常量 65535；0 应解释为"不限"（fo-dicom `DicomService.cs:1813` 注释 `if the requested max pdu size is 0, then we use the configured default MaxSize`）。若确实要保留 65535 的实现上限，则必须在 RQ 里把通告值钳到 65535，使自报值与实际可接收值一致——**最低成本的临时修法是钳制通告值，这也是唯一不需要改 enforce 逻辑的选项**。

**为什么测试没抓到**：`transport.rs:147` `oversized_pdu_is_a_protocol_error` 把 0x00010000 当"非法"来断言，等于把这个 bug 固化成了期望行为；`server.rs:2764` `loopback_honours_a_small_peer_max_pdu` 只测 64 字节的小值方向。没有任何测试验证"通告 131072 → 接收 131072"。`pdu.rs:1123` `max_pdu_length_is_big_endian` 只验证编码方向的大端序，不验证强制上限。

---

### 2.6 【中】`push_item` 无 u16 长度溢出保护，超长 A-ASSOCIATE 会静默写出错误长度

**位置**：`pdu.rs:585-594`

```rust
fn push_item(out: &mut Vec<u8>, item_type: u8, value: &[u8]) {
    let pad = value.len() & 1;
    out.push(item_type);
    out.push(0);
    out.extend_from_slice(&((value.len() + pad) as u16).to_be_bytes());  // ← 截断，无检查
```

**为什么错**：`value.len() + pad` 超过 65535 时 `as u16` 静默回绕，写出与真实内容不符的 item 长度，产生结构损坏的 PDU。`encode()` 的 PDU 总长同样用 `body.len() as u32`（`pdu.rs:575`），无上界检查。

**可达性**：单个 item 超 64 KiB 需要极长的 UID，但 **A-ASSOCIATE 整体超 65535 很容易**——每个 presentation context 约 60–100 字节，约 700 个 context 即可越界；`ContextIdAllocator` 上限是 128 个（`association.rs:91`），加上 user information 里的 role selection / extended negotiation 子项各占几十字节，128 个 context 配角色选择时整体长度可逼近甚至超过 65535。

**正确行为**：`push_item` 在 `value.len() + pad > u16::MAX` 时返回错误；`encode_associate` 在总长超限时返回 `Protocol` 错误。fo-dicom 的做法是分配 `ushort.MaxValue` 的缓冲并在越界时抛异常（`PDU.cs:536-538`）。

**为什么测试没抓到**：`pdu.rs:840` `associate_item_values_are_even_length_on_the_wire` 只验证偶数长度，不验证长度正确性；无任何"超长关联"用例。

---

### 2.7 【中】`Priority` 数值映射与默认值均不符合 PS3.7 / fo-dicom

**位置**：`dimse.rs:47-73`、`dimse.rs:327`

```rust
pub enum Priority { Low, #[default] High, Medium }
Priority::Low => 0, Priority::High => 1, Priority::Medium => 2,
// from_dataset 缺省：Priority::from_code(ds.get_or(tag::PRIORITY, 1u16))
```

**为什么错**：PS3.7 Table E.1-1 与 `DicomPriority.cs` 定义为 `Low = 0x0002, Medium = 0x0000, High = 0x0001`；DIMSE 未指定优先级时语义为 MEDIUM。

radicom 的映射是 `Low=0, High=1, Medium=2`：
- `Low` 发出 0，对端读作 MEDIUM；
- `Medium` 发出 2，对端读作 LOW；
- `High` 恰好正确（1），但默认值也是 High；
- `from_dataset` 缺省取 1 → High，而规范缺省是 Medium。

即三种优先级里两种映射错误，且缺省行为把"未指定"变成"最高优先级"。

**正确行为**：`Low=2, Medium=0, High=1`；`#[default]` 为 `Medium`；缺省读取码 `0`。

**为什么测试没抓到（且被反向固化）**：`dimse.rs:554-565` `priority_uses_the_standard_numeric_order` 断言 `Low==0 / High==1 / Medium==2`，注释还写着"the standard order is low(0) < high(1) < medium(2)"——这是一个把错误当作标准断言的 false-green 测试。

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

**正确行为**：保留每个数值 + service class 组合的独立命名与描述，或至少提供 `(code, service_class) -> description` 查询；缺失的 `DicomStatus.Lookup` 描述表见 §1.4。

---

### 2.9 【中】`AbortSource::Unknown = 1` 把保留值当有效来源

**位置**：`error.rs:65-93`，注释见 `error.rs:70`（`/// 1 — reserved / unknown, treated as provider.`）

**为什么有问题**：PS3.8 §9.3.8 定义 A-ABORT source 为 `0 = user, 2 = provider`，`1` 是保留值。radicom 把 `1` 映射成具名变体 `Unknown` 并在 `from_code` 中接受，于是收到 source=1 的畸形 PDU 也被当作合法解析通过，而注释声称"treated as provider"在代码里并无任何对应处理（`code()` 仍回写 1）。

**正确行为**：`from_code(1)` 返回 `None`（走 `Protocol` 错误）或映射到明确的 `Reserved` 变体且不当成 provider；回写时不应产生 source=1。

**测试**：`error.rs:152` `abort_codes_round_trip` 只做 `from_code(code())` 自往返，等于把"1 是合法值"固化。

---

### 2.10 【中】`ContextState::Proposed` 的 `result_code()` 返回 0（Accept）

**位置**：`association.rs:49-59`

```rust
ContextState::RejectTransferSyntaxesNotSupported => 4,
// a proposed context never appears on the wire as an AC result
ContextState::Proposed => 0,
```

**为什么有问题**：fo-dicom 显式区分 `Proposed = 255`（`DicomPresentationContext.cs`），正因为"尚未协商"与"已接受"是两种不同状态。radicom 让 `Proposed.result_code() == 0`，任何走 `result_code()` 的路径（例如拼装 AC 时误用了未协商状态的 context）都会写出"接受"，且类型系统无法区分。

**正确行为**：`result_code()` 对 `Proposed` 返回 `None`（改为 `Option<u8>`）或让该状态在编码路径上不可达（例如 AC 编码只接受已协商类型）。

---

### 2.11 【中】N-GET / N-SET / N-ACTION / N-DELETE 请求发的是 Affected* 而不是 Requested*

**位置**：`nservice.rs:33-34`（结构体无 `requested_*` 字段）、`nservice.rs:57-58`（`to_command` 只赋 affected）、`nservice.rs:124-125`（响应同样）

**为什么错**：PS3.7 §10.1 规定这四个服务的请求命令必须携带 `(0000,0003) Requested SOP Class UID` 与 `(0000,1001) Requested SOP Instance UID`；只有 N-CREATE 与 N-EVENT-REPORT 用 `(0000,0002) Affected SOP Class UID` / `(0000,1000) Affected SOP Instance UID`。fo-dicom 的判定逻辑集中在 `DicomMessage.cs:60-95`（switch on `Type`），各请求类分别印证：`DicomNGetRequest.cs:45`、`DicomNSetRequest.cs:24`、`DicomNActionRequest.cs:51`、`DicomNDeleteRequest.cs:24` 全部读写 `RequestedSOPInstanceUID`。

`dimse.rs:196-221` 的 `Command` 结构本来**已经有** `requested_sop_class_uid` / `requested_sop_instance_uid` 字段，`dimse.rs:242-244` 与 `dimse.rs:269-271` 也会正确编码这两个 tag——但 `NServiceRequest` 从不设置它们，只设 `affected_*`。

**实际后果**：radicom 发出的 N-GET/N-SET/N-ACTION/N-DELETE 请求在 fo-dicom 服务端会被判为缺少必需属性，返回 `0x0122`（SOP class not supported）或直接命令集解析失败；反之收到的合法 N 请求也读不到 SOP Instance UID（`from_command` 只读 `affected_*`），MPPS / Storage Commitment / UPS 等所有 DIMSE-N 服务的实例定位全部失效。

**正确行为**：`to_command` / `from_command` 按 `field` 分流——`NGetRq|NSetRq|NActionRq|NDeleteRq` 用 `requested_*`，`NCreateRq|NEventReportRq` 用 `affected_*`；响应侧统一回显 `affected_sop_instance_uid`（PS3.7 §10.1 规定响应带 Affected）。

**为什么测试没抓到**：`nservice.rs:160` `a_request_command_round_trips_through_its_dataset` 用 N-ACTION 但只设 `affected_*`，然后自己读自己；`nservice.rs:195` 同理。全文件无一处按 PS3.7 表校验 tag 选择。

---

### 2.12 【中】`apply_options` 静默丢弃 `send_buffer_size` / `receive_buffer_size`

**位置**：`transport.rs:67-73`

```rust
stream.set_nodelay(options.nodelay).map_err(NetworkError::Io)?;
// buffer sizing is advisory: platforms may refuse it, which is not fatal
let _ = options.send_buffer_size;
let _ = options.receive_buffer_size;
Ok(())
```

**为什么错**：`TcpOptions` 公开声明了这两个字段（`transport.rs:20-22`），默认 `None`，但设置后完全无效——这不是"平台拒绝时忽略"，而是**从不尝试**。调用方以为调优生效，实际没有。fo-dicom 通过 `NetworkStreamCreationOptions.SendBufferSize/ReceiveBufferSize` 真正设置 socket 选项。

**正确行为**：调用 `socket2` 或 tokio 的 `set_recv_buffer_size` / `set_send_buffer_size`（tokio `TcpStream` 未直接暴露时可用 `AsRawSocket` + `socket2`），失败时按注释所说降级而非报错。

**为什么测试没抓到**：`transport.rs:167` `tcp_options_default_to_nodelay` 只断言默认值是 `None`——恰好让"字段被丢弃"这件事看起来正常。没有任何测试设置非 `None` 值再观察行为。

---

### 2.13 【中】TLS 未显式声明最低协议版本，且服务端不支持 mTLS

**位置**：`tls.rs:54-57`、`tls.rs:70-79`

```rust
let config = ServerConfig::builder()
    .with_no_client_auth()          // ← 恒定，无法配置客户端证书
    .with_single_cert(chain, key)
```

**为什么有问题**：
1. **协议版本**：fo-dicom 在 `DefaultTlsAcceptor` / `DefaultTlsInitiator` 中都显式设定 `SslProtocols.Tls12 | SslProtocols.Tls13`。radicom 依赖 rustls 的编译期默认下限（当前版本为 TLS1.2），行为随依赖升级而变，且代码里没有可审计的声明。医疗场景通常要求"最低 TLS1.2"可被明确验证。
2. **mTLS**：fo-dicom `DefaultTlsAcceptor.RequireMutualAuthentication`（默认 false，但可开启）。radicom 硬编码 `with_no_client_auth()`，`TlsOptions` 里也没有对应字段，**无法**启用客户端证书校验。

**正确行为**：用 `ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13, &rustls::version::TLS12])` 显式声明下限；为 `TlsOptions` 增加 `client_ca_file` / `require_client_auth`，非空时改用 `with_client_cert_verifier(...)`。

---

### 2.14 【中】客户端证书名默认 `"localhost"`，与连接目标无关

**位置**：`tls.rs:84-86`

```rust
pub fn server_name(&self) -> String {
    self.server_name.clone().unwrap_or_else(|| "localhost".to_string())
}
```

**为什么有问题**：TLS 证书校验的 SNI 与主机名必须匹配**实际连接的目标**。`TlsOptions::connect` 只接收已建立的 `TcpStream`（`tls.rs:96-105`），拿不到目标地址，于是未显式设置 `server_name` 时一律用 `"localhost"`。连接 `pacs.hospital.example` 时会用 `localhost` 去比对证书，结果：真实证书必然校验失败（可用性事故），而任何签给 `localhost` 的证书都会被接受（安全隐患）。

**正确行为**：`connect` 应接收目标主机名（或 `ServerName`）参数并强制要求；确需默认值时，应从 `TcpStream::peer_addr()` 反查（仅 IP 可用时用 `ServerName::IpAddress`），而不是硬编码 `localhost`。

**为什么测试没抓到**：`tls.rs:305` `server_name_defaults_to_localhost` 把这个默认值当作规格断言；`tls.rs:223` `tls_loopback_c_echo_succeeds` 恰好也是 `localhost`，两者互相印证了一个在真实部署里必然出问题的行为。

---

### 2.15 【低】AE title 截断按 `char` 而非字节，且无字符集校验

**位置**：`pdu.rs:305-317`

```rust
fn into_ae(value: impl Into<String>) -> String {
    value.chars().take(16).collect()          // 16 个 char，可能是 48 字节
}
fn encode_ae(value: &str) -> [u8; 16] {
    for (i, b) in value.as_bytes().iter().take(16).enumerate() { out[i] = *b; }  // 按字节截断
}
```

**为什么有问题**：`into_ae` 保留 16 个**字符**，`encode_ae` 按 16 **字节**截断。非 ASCII AE title（例如中文）会得到 16 字符的字符串，写出时被截断到 16 字节，且可能在 UTF-8 字符中间切断。PS3.8 §9.3.2 规定 AE title 为 16 字节、字符集受限（不允许反斜杠、控制字符；`Associate::new` 未做任何校验）。

**正确行为**：以字节为单位截断到 16，并校验字符集（可打印 ASCII 除 `\`）；超长时返回错误而非静默截断（fo-dicom 是静默 `Substring` + `PadRight`，但至少是字节等价且不跨字符集破坏）。

**测试**：`pdu.rs:823` `ae_titles_are_padded_and_trimmed` 用纯 ASCII `"A-VERY-LONG-AE-TITLE"`，掩盖了 char/byte 不一致。

---

### 2.16 【低】响应命令集仍写入 `(0000,0110) Message ID`

**位置**：`dimse.rs:245`（`to_dataset` 无条件写 MESSAGE_ID）、`nservice.rs:121`

```rust
insert_us(&mut ds, tag::MESSAGE_ID, self.message_id);   // 无 Option 判断
// nservice.rs:121
let mut command = Command::request(self.field, 0);      // 响应以 message_id = 0 起步
```

**为什么有问题**：PS3.7 Table E.1-1 中 Message ID `(0000,0110)` 只出现在**请求**里；响应只带 Message ID Being Responded To `(0000,0120)`。fo-dicom `DicomResponse` 只设 `RequestMessageID`（`DicomResponse.cs:40`、`DicomResponse.cs:47-51`）。radicom 的响应会额外发出 `(0000,0110) = 0`。

对端通常忽略该冗余字段，故定为低。但严格校验的实现可能告警。

**正确行为**：`Message::message_id` 改为 `Option<u16>`，或 `to_dataset` 仅在 `field.is_request()` 时写 MESSAGE_ID。

---

### 2.17 【低】`decode_app_info` / `decode_common_extended` 遇越界静默 `break`

**位置**：`pdu.rs:485-487`、`pdu.rs:505-507`

```rust
if start + len > raw.len() { break; }     // 静默丢弃剩余内容
```

**为什么有问题**：长度越界说明对端发来畸形数据，应当报 `Protocol` 错误（与 `item_header` 的行为一致，`pdu.rs:603-605` 就返回错误）。这里静默 `break` 让解析"成功"返回部分字段，把协议违规降级成数据丢失，排查困难。

**正确行为**：与 `item_header` 对齐，返回 `NetworkError::protocol("app info field runs past the item")`。

---

### 2.18 【低】`extended.rs:245-250` `apply_answer` 无条件清空 `common_extended`

**位置**：`extended.rs:245-250`

```rust
pub fn apply_answer(user_information: &mut UserInformation, policy: &ExtendedNegotiationPolicy) {
    let answered = policy.answer(&user_information.extended_negotiations);
    user_information.extended_negotiations = answered;
    user_information.common_extended = None;   // 无条件
}
```

**为什么有问题**：注释说"common extended negotiation is RQ-only (PS3.7), so the AC never echoes it"。但本函数接收的是 `&mut UserInformation`，调用方无法从签名看出它**只能**用在 AC 方向。若被误用于 RQ（例如构造转发场景或复用一个 UserInformation），会静默丢掉本应发送的 0x57 子项。

**正确行为**：拆成显式的 `apply_answer_to_ac(...)`，或在文档/类型上约束方向；至少不应在"应用答案"这个动作里顺手改无关字段。

**测试**：`extended.rs:375` `common_extended_is_rq_only` 断言的正是这个行为，等于把它固化为规格。

---

## 3. 性能问题

### 3.1 【高】`read_pdu` 每 PDU 两次分配 + 一次整帧拷贝

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

### 3.2 【高】`write_pdu` 每 PDU 重新编码整帧

**位置**：`transport.rs:59-64`

```rust
let bytes = pdu::encode(pdu);          // 每次分配并复制全部 PDV 数据
writer.write_all(&bytes).await?;
writer.flush().await?;                  // TcpStream 上 flush 是空操作
```

`pdu::encode`（`pdu.rs:563-578`）先算 body 再 `extend_from_slice`，而 `encode_pdata`（`pdu.rs:716-737`）对每个 PDV 又 `extend_from_slice(&pdv.data)`。但 `Pdv.data` 本身已经是从消息里 `to_vec()` 复制来的独立缓冲（见 3.3），所以一次发送至少要经历：原始消息 → `Pdv.data` 拷贝 → `encode` 输出拷贝 → socket。每 PDV 两次冗余全量拷贝。

**最小修复**：给 `write_pdu` 增加"已编码缓冲"路径（PDV 切片直接 `write_vectored`），或让 `Pdv` 持有 `Bytes` 借用而非 `Vec<u8>` 复制。`flush()` 对 `TcpStream` 无意义，可直接删掉。

### 3.3 【中】消息在发送路径上被复制至少 3 次

**位置**：`message.rs:50-59` → `assembler.rs:123-159`（`data[offset..end].to_vec()`）、`pdu.rs:545`（`value.to_vec()`）、`pdu.rs:732`（`extend_from_slice`）

- `encode_command` / `encode_dataset` 各分配一个完整消息缓冲（`message.rs:18`、`message.rs:34`）；
- `split` 对每个分片 `data[offset..end].to_vec()`（`assembler.rs:155`），再复制一遍；
- `encode_pdata` 写出时第 3 次复制。

**最小修复**：`Pdv.data` 改为 `bytes::Bytes`（零拷贝切片），`split` 直接 `Bytes::slice`。

### 3.4 【中】解码路径每 PDV 一次 `to_vec`

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

### 3.7 【低】`transport.rs:79` `network_identifier` 每次调用都读环境变量并做 u128 哈希

非热点路径（"nothing in the engine depends on it"，`transport.rs:77-78` 注释），但既然要保留，用 `OnceLock` 缓存即可，以免被误用在每连接初始化路径。

---

## 4. 测试审查

统计：`radicom-network` 共 248 个测试通过。以下仅针对在范围内的文件，按"是否会放过真实缺陷"分类。

### 4.1 False-green：断言了错误行为

| 测试 | 位置 | 问题 |
| --- | --- | --- |
| `priority_uses_the_standard_numeric_order` | `dimse.rs:554` | 断言 `Low==0 / High==1 / Medium==2` 并把注释写成"the standard order"，而 PS3.7/fo-dicom 是 `Low=2 / Medium=0 / High=1`。把 §2.7 的错误固化为规格。 |
| `server_name_defaults_to_localhost` | `tls.rs:305` | 断言证书校验名默认 `"localhost"`，固化 §2.14 的可用性/安全双重问题。 |
| `oversized_pdu_is_a_protocol_error` | `transport.rs:147` | 把"长度 0x00010000 必须报错"当期望，固化 §2.5 的 65535 硬上限。 |
| `tcp_options_default_to_nodelay` | `transport.rs:167` | 只断言 buffer size 默认为 `None`；因 §2.12 中该字段被丢弃，任何断言都无从触发，恰好为缺陷提供了"测试通过"的掩护。 |
| `app_info_keeps_field_order_and_replaces_in_place` | `extended.rs:284` | 断言"按插入序、末次值胜出"的 `Vec` 模型，而 fo-dicom 是 `SortedDictionary` + 空洞填充；固化 §2.3 的内存模型偏差。 |
| `common_extended_is_rq_only` | `extended.rs:375` | 断言 `apply_answer` 无条件清空 `common_extended`，固化 §2.18 的隐式副作用。 |
| `abort_codes_round_trip` | `error.rs:152` | 用 `from_code(code())` 自往返验证 `Unknown=1` 合法，固化 §2.9。 |

### 4.2 同义反复：自编码→自解码，无法发现线格错误

这一组是 §2.1–§2.4 四个高危缺陷能存活至今的**根本原因**：测试只验证"radicom 能读懂 radicom"，对"是否与 PS3.8 一致"零覆盖。

| 测试 | 位置 | 被掩盖的缺陷 |
| --- | --- | --- |
| `associate_ac_carries_results_and_one_syntax` | `pdu.rs:802` | §2.1 result 偏移（无字节级断言） |
| `associate_ac_omits_abstract_syntax_and_carries_one_syntax` | `pdu.rs:884` | §2.1 同上 |
| `associate_rq_round_trips` | `pdu.rs:776` | §2.2/§2.3/§2.4 全部（断言字段回读相等，不看字节） |
| `user_information_sub_items_round_trip` | `pdu.rs:922` | §2.2 角色布局、§2.3 app info、§2.4 user identity |
| `extended_negotiation_lands_on_the_side_matching_the_pdu` | `pdu.rs:964` | §2.3 app info 三元组格式 |
| `user_information_carries_identity_and_roles` | `association.rs:679` | §2.2 / §2.4 |
| `role_selections_survive_the_answer` | `extended.rs:400` | §2.2 |
| `every_proposed_context_gets_an_answer` | `association.rs:550` | §2.1（只查状态机，不查字节） |
| `context_state_codes_round_trip` | `association.rs:606` | §2.10 `Proposed → 0` |
| `a_request_command_round_trips_through_its_dataset` | `nservice.rs:160` | §2.11 N-ACTION 用 Affected* |
| `a_response_command_round_trips` | `nservice.rs:195` | §2.11 |
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

1. **A-ASSOCIATE 的字节级 golden test（最高优先）**：这是唯一能捕获 §2.1–§2.4 的手段，因为自往返测试对它们**原理上无效**。建议直接用 §2.1 那条 176 字节 AC fixture 作为起点，再补一条含 role selection + user identity + extended negotiation 的 RQ fixture（§2.2/§2.3/§2.4 的字节见各条）。断言点：
   - AC：`pc_item[2] == result`（偏移 2，不是偏移 1）；
   - role：`item[0..2]` 是 UID 长度、UID 从偏移 2 开始、两个角色字节在 `2+len` 与 `3+len`；
   - ext neg：application information 长度 == 字段数（扁平），不是三元组；
   - user identity：`item[1]` 是 positive 标志，`item[2..4]` 是 primary 长度。
2. **max-PDU > 65535 的读路径**：构造长度 131072 的 P-DATA-TF，验证 `read_pdu` 不报错（当前 §2.5）。这条能直接锁住"通告值 vs 强制上限"的不一致。
3. **N-Service tag 选择**：按 PS3.7 §10.1 表逐一断言四个服务用 `(0000,0003)/(0000,1001)`、两个服务用 `(0000,0002)/(0000,1000)`（当前 §2.11）。
4. **`apply_options` 的生效验证**：设置非 `None` 的 buffer size 后确认 `SO_SNDBUF` 实际改变（当前 §2.12）。
5. **`push_item` 超长路径**：构造单个 item > 65535 与整体 A-ASSOCIATE > 65535，断言返回错误（当前 §2.6）。
6. **AE title 非 ASCII**：中文/多字节输入下断言恰好 16 字节且不跨字符边界（当前 §2.15）。
7. **PDV 的 context id 合法性**：`decode_pdata` 不校验 `context_id` 为奇数且 1..=255，应加负向用例。
8. **分片边界的精确性**：`message.rs:118` `message_splitting_puts_the_command_first` 只断言 PDU 数量与位标志，未验证每个 PDU 的总长 ≤ max_pdu（含 6 字节头）。

### 4.5 一条方法论结论

§2.1–§2.4 四类缺陷的共同特征是：**编码器与解码器共享同一个错误假设，因此自往返测试 100% 通过**。这四项目前全部是"测试全绿但协议不兼容"。

由此得出的可推广结论：协议层的正确性不能由自往返测试保证，必须由**外部派生的 golden 字节**（标准条文或第三方实现的输出）保证。建议把 §4.4-1 作为协议层的准入条件——凡是新增/修改 A-ASSOCIATE 子项的改动，都必须同步更新 golden fixture。

---

## 附：建议修复顺序

1. **§2.1 / §2.2 / §2.3 / §2.4** —— 四处线格错误（均已字节验证），改动都局限在 `pdu.rs` 的 encode/decode 对内，一晚可改完。配 §4.4-1 的 golden test 即可长期防回归。这是当前与任何第三方 PACS 互通的最大阻塞：**这四项不修，radicom 无法与 fo-dicom / DCMTK 建立一条功能完整的关联**。
2. **§2.5** —— max-PDU 通告与强制上限自相矛盾。最低成本修法是钳制通告值；根治是让读侧上限取协商值。
3. **§2.11** —— N-Service 全系列不可用的根因，改动局限在 `nservice.rs`。
4. **§2.7** —— 一行常量修正，影响所有 DIMSE 请求的优先级语义。
5. **§2.12 / §2.13 / §2.14** —— 部署可信度与安全性。
6. §3.1 / §3.2 —— 吞吐优化，改动局部且可独立验证。
7. 其余中/低项按需。

> 关于 §2.1–§2.5 的修复验证方式：不要用"改完自往返通过"作为完成标准——那正是当初漏掉这四个 bug 的原因。完成标准应当是 §4.4-1/§4.4-2 的 golden fixture 通过。

---

*评审范围限于协议层文件，未覆盖 `server.rs`、`client.rs`、`engine.rs`、`cfind.rs`、`cget.rs`、`cmove.rs`、`store.rs`、`services.rs`、`hosting.rs`、`assembler.rs`、`expert.rs`、`observability.rs`（由另一任务负责）。本文对 `assembler.rs` / `server.rs` 的引用仅作为调用点可达性证据。*
