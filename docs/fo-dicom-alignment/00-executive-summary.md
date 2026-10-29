# radicom 代码走查总报告（对照 fo-dicom）

> 状态提示：本文件是**走查当时的快照**，其中列出的多数“高危缺陷/缺失”此后已修复。逐条当前处置与证据
> 见 [`PROGRESS.md`](PROGRESS.md) 与源码/测试；本文正文保留原始结论用于追溯。


本文件是五份分模块详细评审的**汇总**，回答三个问题：与 fo-dicom 相比功能是否缺失、代码是否正确/有无性能问题、测试是否正确。

- 审查对象：`D:\Code\rust-code\radicom`（git `9347548`，`master`）
- 参照实现：本地编译的 **fo-dicom 6.0**（`D:\Code\dotnet-source\fo-dicom`，.NET 10 SDK），用它作为差分测试的 oracle
- 排除范围：图像编解码器（JPEG / JPEG-LS / J2K / HTJ2K / RLE 的编解码实现）按用户要求不审
- 审查方式：逐行读代码 + **实际运行验证**。凡行为争议均用真实 `.dcm`、最小字节夹具或与 fo-dicom 的同输入对比实测，探针放在系统临时目录，**仓库源码与测试零改动**（`git status` 中 `docs/review/` 为唯一新增）

分报告：

| 报告 | 范围 |
| --- | --- |
| [`01-model-io.md`](01-model-io.md) | `model/` `io/` `encoding/` `validation/` |
| [`02-pixel-render.md`](02-pixel-render.md) | `pixel/` `render/`（不含 codec 实现） |
| [`03-network-protocol.md`](03-network-protocol.md) | `radicom-network`：PDU/传输/TLS/关联/DIMSE 命令字 |
| [`04-serialization-media-sr-anon-tests.md`](04-serialization-media-sr-anon-tests.md) | `media/` `sr/` `anonymizer/` `rules/` `dump/` `radicom-json` + **全 workspace 测试审计** |
| [`05-network-service.md`](05-network-service.md) | `radicom-network`：server/client/engine/C-FIND/C-GET/C-MOVE/store/hosting |

**基线（Lead 统一验证）**：`cargo build --workspace --all-targets`、`cargo clippy --workspace --all-features --all-targets`、`cargo test --workspace --all-features` 全部通过——radicom 497 + network 248 + json 34 + dict 4 + 集成测试，**0 失败**。

---

## 结论摘要

**`docs/notes/01-fo-dicom-alignment-audit.md:18` 声称「P0/P1 及 P2 核心功能全部 DONE」，这个结论不成立。** 本次走查在「全部 DONE、17 个套件全绿」的前提下，找到了 **14 个高危缺陷**，其中 3 个属于**在真实文件上必然失败**的级别：

1. **渲染像素通路对真实解析文件完全失效**（`pixel/mod.rs:126-132`）。`width()`/`height()` 用 `u32` 读 US 类型标签，而解析后的元素是原始字节，`read_le_unit` 要求 4 字节 → 返回 0。后果是 `uncompressed_frame_size()` 恒为 0，`get_frame(0)` **静默返回 0 字节缓冲区**，`render_frame` 直接报错。fo-dicom 同样文件正常出 512×512。**这不是"少个功能"，是核心通路在真实数据上不可用。** 唯一的根因只需把这两行改成读 `u16` 即可修复（实测改后 ct.dcm 立刻渲染 512×512）。
2. **解析器无法打开 fo-dicom 测试集中的 10 个文件**，其中 2 个直接 **panic**。已定位到 3 个根因（片段分隔符少读 4 字节、CP-246 条件写反、私有序列探测缺失）。
3. **A-ASSOCIATE 的 4 个子项线格全部错误**，用标准字节验证：任意合规对端都无法完成 role selection / extended negotiation / user identity 协商，且**被拒绝的 presentation context 会被误判为接受**（含安全含义）。

其余高危项：任一响应型 command field 可触发服务端 panic（未认证、64 字节即可触发）、字符集整表错映射导致俄/阿/希腊/希伯来/土耳其文乱码、DICOMDIR 带 icon 时 root offset 写错、`CodeItem` 的 tag 模型整体错误、JSON 未剥离 group length（违背自身文档承诺）、私有 tag VR 恒为 UN、VOI LUT 16-bit 用 `>>8` 导致窗口映射错误、overlay 三处（标签/行列序/位序）、print 两个 tag 常量错导致空白胶片、C-MOVE 计数归零、默认 C-STORE sink 泄漏临时文件、`max_clients` 完全失效。

**测试方面是本次最严重的系统性问题**：799 个测试函数全绿，但上述高危缺陷**没有一个是测试能发现的**。测试体系的判别力不足以支撑"全部 DONE"的结论——详见 §4。

| 级别 | 数量 | 代表项 |
| --- | --- | --- |
| 高 | 14 | 渲染维数为 0（静默丢数据）、10 个文件解析失败/panic、A-ASSOCIATE 4 处线格、字符集乱码、DICOMDIR offset、CodeItem tag、JSON group length、服务端 panic、VOI LUT `>>8`、overlay 三处、print tag 常量、C-MOVE 计数归零、临时文件泄漏、`max_clients` 失效 |
| 中 | 约 55 | 见各分报告 |
| 低 | 约 40 | 见各分报告 |

**一个反复出现的元模式**（值得单独记住）：多处缺陷都是「读写共用同一个错误常量/公式，因此单元测试自洽通过，真实数据失败」。已确认的同类项：`CodeItem` 的 tag 家族、print 的 `(2010,0120)`/`(2010,0500)`、overlay 的 `(60xx,0052)`、charset 的 `charset.rs:437` 断言、VOI LUT 的 `render/mod.rs:1521-1527`、DICOMDIR 的 offset 自扫描。**修复时优先找这类"自洽的错"，它们全部逃过了 799 个测试。**

---

## 1. 功能缺失（对照 fo-dicom）

按用户要求排除 codec 编解码实现后，**骨架层面的覆盖度是高的**——fo-dicom 的主要模块在 radicom 都有对应物，且不少设计意图清楚（两遍 offset 重建、Bump 分配池、PDU 分帧、DIMSE 命令字编码等与 fo-dicom 逐字一致）。真正的差距集中在三类：

### 1.1 有符号但行为不成立（比"缺失"更危险）

| 能力 | fo-dicom | radicom | 后果 |
| --- | --- | --- | --- |
| 解析中的私有序列探测 | `DicomReader.cs:1099 IsPrivateSequence` / `:1124 IsPrivateSequenceBad` / `:365 handleBadPrivateSequence` | **无对应实现**（`reader.rs:431-446` 用永真探测代替） | implicit-VR + 坏私有序列文件必然跑飞（GH626 等） |
| 私有 tag 按私有字典取 VR | `DicomReader.cs:393-409` | `reader.rs:187-207` 对私有 tag 直接返回 `Un` | 所有私有元素类型访问失效 |
| `Suspended` / `IsPartial` | `DicomFile.cs:250` | `ReadOutcome` 只有 Success/Stopped；`is_partial` 硬编码 `false`（`file.rs:187`、`:234`） | 截断文件整体失败而非部分成功（GH179B） |
| CP-246（UN 定义的 SQ 在 BE 下按 LE 读） | `DicomReader.cs:608-623` 条件为 `UN && !IsPrivate` | `reader.rs:576` 条件**恰好相反**（仅私有） | GH177 读错长度 |
| `LongCodeValue` / `URNCodeValue` | `DicomTagGenerated.cs:513` `(0008,0119)` UC、`:?` `(0008,0120)` UR | **完全未实现**，长值与 URN 都写进 `(0008,0100)` SH | 写出的 SR 不符合 PS3.3 |
| `DicomCodeItem::FromUrn` | `DicomCodeItem.cs:50` | 已实现（原为缺失） | `sr/code_item.rs` `from_urn`（`2ec3a5d`） |
| N-GET/N-SET/N-ACTION/N-DELETE 的 Requested* 字段 | PS3.7 表 E.1-1 | 用 `Affected*` 代替（`nservice.rs:33-34,57-58,124-125`） | **DIMSE-N 全系列不可用** |
| `DicomSummary` / 结构化富结果等便捷层 | 多处 | 部分缺失 | 见各分报告 |

### 1.2 明确缺失（走查快照；以下四项现已实现）

- 写入侧 `LargeObjectSize` 可配置阈值 —— 已实现（`WriteOptions::large_object_size`）
- `DicomDatasetComparer` 的可配置语义 —— 已实现（`Dataset::content_equals`，`2ff0e69`）
- `DicomTag.ToString("G"/"X"/"g")` 变体 —— 已实现（`0e51b55`）
- `JsonOptions::validate_on_read` —— 已实现（`4710dde`）

### 1.3 有意的架构分歧（非缺陷）

`docs/development/02-feature-list.md` 里已声明的分歧（D7 四套 .NET 图像后端合并为一套、D11 不做第二个 JSON 后端、GUI dump 改为 CLI、不引入 `opentelemetry` 依赖）判定为**合理**，不列为缺失。

---

## 2. 正确性缺陷（高危 14 项）

### P0-1 渲染/像素通路在真实文件上返回空数据

| | |
| --- | --- |
| 位置 | `pixel/mod.rs:126-132` `width()`/`height()`；`model/element.rs` `read_le_unit` |
| 证据 | 解析后的 `ct.dcm`（512×512×16-bit，PixelData 完好 524288 字节）：`get_frame(0)` → **0 字节**，`render_frame` → `Data(Other{"frame has zero width or height"})`。fo-dicom 同文件 → 512×512 正常出图 |
| 根因 | `width()` 用 `get_or(COLUMNS, 0u32)`；Rows/Columns 是 VR US，解析后为 `ElementValue::Bytes`，走 `read_le_unit::<u32>`，其 `unit = vr.unit_size().max(T::SIZE)` = `max(2,4)` = 4，但单值只有 2 字节 → `IndexOutOfRange{index:0,count:0}` → 回落默认值 0 |
| 放大因素 | `frame_size = 0` 使 `pixel/mod.rs:232` 的边界检查 `offset + frame_size > buffer.size()` 恒为假（`0+0 > N`），于是**返回零长度窗口且不报错**——静默数据丢失而非报错 |
| 连带 | `uncompressed_frame_size()` 恒 `Ok(0)`；`render/icon.rs:55` 图标生成器在真实文件上 `Ok(None)`；`render/reconstruction.rs:337` MPR 槽位数为 0；`mod.rs:227` 转码 frame-size 计算 |

### P0-2 解析器打不开 fo-dicom 测试集的 10 个文件（2 个 panic）

| | |
| --- | --- |
| 证据 | 79 个 fo-dicom `.dcm` 中 fo-dicom 全部可解析，radicom 失败 10 个：`GH626` `GH133` `GH179B` `GH177_D_CLUNIE_CT1_IVRLE_BigEndian_undefined_length` `GH1986` `CR-ModalitySequenceLUT` `OutOfBoundsOverlay` `D_CLUNIE_CT1_RLE_FRAGS` 解析失败；`chrI2.dcm` `chrKoreanMulti.dcm` **panic** |
| 根因 1（覆盖 5 个文件） | `reader.rs:495` 的 `FFFE,E0DD` 分支 `break` 时**未消费 4 字节长度字段**（而 `FFFE,E000` 分支在 `:477` 消费了）→ 流停在 delimiter+4，下一次 `step()` 把 `00 00 00 00` 读成 tag `(0000,0000)` → 按组长度 VR=UL 读出巨长 → `Eof` |
| 根因 2 | `reader.rs:576` CP-246 字节序切换条件与标准相反（见 §1.1），GH177 `needed = 0x58000000`，正是 item 长度 `58 00 00 00` 被按大端读 |
| 根因 3 | 私有序列探测整体缺失（`IsPrivateSequence`/`IsPrivateSequenceBad`/`handleBadPrivateSequence`），GH626 类文件必然跑飞 |
| 注 | GH179B 严格说不是解析 bug：fo-dicom 自己的测试（`Bugs/GH179.cs:28`）断言该文件 `IsPartial == true`，radicom 缺 `Suspended` 语义 |

### P0-3 字符集：ISO_IR 整表映射到 Windows 代码页 + 确定性 panic

| | |
| --- | --- |
| 位置 | `encoding/charset.rs:16-50`（`BUILTINS`）、`:176-203`（`decode_korean_iso2022`）、`:253-263`（`encoding_for_escape`） |
| 表错误 | `ISO_IR 109→windows-1253`（应 ISO-8859-3）、`110→windows-1251`（应 8859-4）、`126→windows-1253`（应 8859-7）、`127→windows-1256`（应 8859-6）、`138→windows-1255`（应 8859-8）、`144→windows-1251`（应 8859-5）、`148→windows-1254`（应 8859-9）、`166→windows-874`（应 TIS-620/8859-11）。fo-dicom `DicomEncoding.cs:143-178` 用 iso-8859-x |
| 实测乱码 | `chrRuss.dcm` `ISO_IR 144` 的 PN 字节 `BB EE DA 63 65 DC D1 79 70 D3` → radicom 出 `»оЪceЬСypУ`，fo-dicom 出 `Люкceмбypг`；`chrArab.dcm` `ISO_IR 127` 字节 `E2 C8 C7 E6 EA 5E E4 E6 D2 C7 D1 20` → radicom 出 `âباوê^نوزار`，fo-dicom 出 `قباني^لنزار` |
| panic | `charset.rs:195` `euc.push(bytes[i] + 0x80)` 在 `ESC $ ) C`（KS X 1001）段内遇到 ≥0x80 的字节时 u8 溢出 → debug panic、release 静默回绕（`0xFB+0x80 → 0x7B` 即 `{`）。最小复现 5 字节 `[1B,24,29,43,FB]` |

### P0-4 A-ASSOCIATE 四个子项线格全部错误（字节验证）

| # | 缺陷 | 位置 | 实测后果 |
| --- | --- | --- | --- |
| 1 | AC 的 presentation context result 写在偏移 1 且偏移 2 恒写 0 | `pdu.rs:366`（读）/`:628`（写） | 构造标准 AC（context 1=ACCEPT(0)，context 3=REJECT(3)）交给 radicom → **两个都报 `accepted=true`**；反向再编码，两个 context 的 result 字节都是 0 → **被拒绝的 context 在双向都被当成接受** |
| 2 | Role Selection（0x54）线格反了 | `pdu.rs:663-670` / `:417-421` | 发 `uid-len(2)+uid+scu+scp`，radicom 解出 `sop="2.840.10008.5.1.4.1.1.4\u{1}\u{1}"`（UID 平移 2 字节）且 `scp` 从 0 读成 `true` → C-GET/C-MOVE 反向 C-STORE 角色协商失效 |
| 3 | Extended Negotiation（0x56）application information 写成三元组 | `pdu.rs:671-681` | 标准扁平数组 `[00,01,04]` → radicom 解出 `requested=[]`，**静默丢弃** → Storage/C-FIND/C-GET 扩展协商全部不可用 |
| 4 | User Identity（0x58）字段顺序错 | `pdu.rs:695-705` | 标准 `type+positive+plen+primary+slen+secondary` → radicom 解出 `primary="" secondary="" pos=false`，**三个 payload 字段全丢** |

补充：`MAX_PDU_SIZE = 65535`（`pdu.rs:10`、`transport.rs:46`）把 A-ASSOCIATE 的 u16 上限套到 P-DATA-TF 的 u32 上；radicom 自报 `max_pdu=131072`（解码正确）后，会拒绝合规对端发来的 131072 字节 PDU——**自报能力后拒绝履行**。

### P0-5 对端可触发的服务端 panic（远程 DoS）

| | |
| --- | --- |
| 位置 | `dimse.rs:225` `response_field(request.field).expect(...)`，触发点 `server.rs:354-359` 兜底分支 → `serving.fail`（`server.rs:752`） |
| 证据 | 真实 TCP 连接后按正常 P-DATA 分帧发一条响应型 command field（如 `0x8020` C-FIND-RSP）：`panicked at dimse.rs:225:51: request field has a response counterpart`，连接静默丢弃、无 A-ABORT，该连接所有在途请求丢失。`panic="abort"` 配置下即进程级 DoS |
| 应当 | PS3.7 §9.3 要求对无法识别的命令回 `0x0110` 或发 A-ABORT，绝不崩溃 |

### P0-6 DICOMDIR 带 icon 时 root offset 写错

| | |
| --- | --- |
| 位置 | `media/mod.rs:673-686` `item_offsets` 的 `positions[positions.len()-count..]`，调用点 `:438-457` |
| 证据 | 4 条记录（PATIENT/STUDY/SERIES/IMAGE）：无 icon 时 `first_root=372` 正确指向 PATIENT；**带 icon 时 `first_root=440` 指向 STUDY**。因为 `IconImageSequence (0088,0200)` 给 IMAGE 记录多加了第 5 个 `FFFE,E000` 头，尾部切片整体错位一条记录 |
| 后果 | 写出的 DICOMDIR 不符合 PS3.3 F.2.2；合规读者（以绝对偏移为键）会拼出缺 PATIENT 层的错树。radicom 自己读不出来——`stitch()` 走同一切片逻辑，错误互相抵消 |

### P0-7 `CodeItem` 的 tag 模型整体错误（3 个独立缺陷）

| | |
| --- | --- |
| 位置 | `sr/code_item.rs:22-36` |
| a | `SHORT` = `(0018,0090)/(0018,0100)/(0018,0102)/(0018,0104)`。经字典核验：`(0018,0090)` 是 `DataCollectionDiameter` **VR DS**——把字符串写进 DS 槽产出**非法数据集**；后三个字典中不存在 → 写出 VR=UN |
| b | `LONG` 的 value/scheme/meaning 正确，但 `scheme_version = (0008,0106)`；该 tag 是 `ContextGroupVersion` **VR DT**，正确应为 `CodingSchemeVersion (0008,0103)` SH |
| c | `LongCodeValue (0008,0119)` UC 与 `URNCodeValue (0008,0120)` UR **从未实现**：17 字符值写进 `(0008,0100)` SH（上限 16，长度违规），URN 也写进 `(0008,0100)` |
| 根本 | DICOM 里**不存在**「short/long 两套 tag 家族」，正确模型是同一组 tag、按值长/URN 选择 value 槽。模块的中心设计前提是错的，所以缺陷是系统性的而非笔误 |

### P0-8 JSON 未剥离 group length（违背自身承诺）

`docs/development/02-feature-list.md:167` JSON-01 明写「group length stripped」，`radicom-json/src/lib.rs:109-123` 实际**完全没剥离**。GH178 输出含 `00080000`/`00100000`/`00180000`/`00200000`/`00280000`/`00400000` 共 6 个 `*0000` 键，嵌套 item 内同样泄漏。附带 3 处分歧：空 SQ 输出 `{"Value":[]}`（fo-dicom 省略 `Value`）、空 CS 分量给 `""` 而非 `null`、IS `"0001"` 保持字符串而非数字 `1`。

### P0-9 私有 tag 的 VR 恒为 UN

`reader.rs:187-207` `dictionary_vr` 对任何非 creator 槽的私有 tag 直接返回 `Vr::Un`，因为签名取 `(u16,u16)` **丢掉了 `resolve_tag` 刚绑定的 creator**。实测 `(0009,1001)` + creator `GEMS_IDEN_01`：`default_dictionary().entry()` 明明返回 `vrs=[Lo]`，解析器仍给 `UN`。导致 ct.dcm/mr.dcm/test_1847.dcm 里所有私有元素类型访问失效（fo-dicom 给 `LO "GE_GENESIS_FF"`、`SS`、`SL` 等）。

### P0-10 VOI LUT 16-bit 输出用 `>> 8`，而非按输出范围重标定

`render/mod.rs:99-105`：PS3.3 C.11.2.1.1 规定 16-bit VOI LUT 输出范围是 `0..2^n-1`，fo-dicom 用 `VOILinearLUT(Min,Max)` + `OutputLUT` 重标定（`GenericGrayscalePipeline.cs:146-147,161`）。实测 LUT `[0,1000]`、输入 1000 → radicom 渲染成 **3**，正确应为 **255**。`render/mod.rs:1521-1527` 的测试**把这个错误断言成期望值**。

### P0-11 overlay 三处错误（标签、行列序、位序）

| 缺陷 | 位置 | 实测 |
| --- | --- | --- |
| `ImageFrameOrigin` 读 `(60xx,0052)/(60xx,0053)` | `pixel/overlay.rs:67` | 字典核实：`(60xx,0051)` 才是 `ImageFrameOrigin`；`0052` 是已淘汰的 `OverlayPlaneOrigin`，`0053` **根本不存在**。实测 Origin=3 时 overlay 画在 frame 0,1 而非 2,3 |
| `OverlayOrigin` 行列顺序颠倒 | `overlay.rs` | `(0050)` 按 PS3.3 C.9.2 是 row\column，代码当 x,y 用 → 落在 (2,0) 而非 (1,2) |
| 位平面按 MSB-first 打包 | `overlay.rs:195` | PS3.5 §8.1.2 要求 LSB-first，fo-dicom 也是 LSB-first。实测写出 `0b10000000`，应为 `0b00000001`。`render/mod.rs:804` 同样自洽错误，所以测试看不出来 |

### P0-12 print 模块两个 tag 常量取值错误

`print/models.rs:39` `EMPTY_IMAGE_DENSITY=(2010,0120)`——字典核实该 tag 是 **`MinDensity`**；`:43` `REFERENCED_IMAGE_BOX_SEQUENCE=(2010,0500)`——实为 **`ReferencedFilmSessionSequence`**，正确的 image box 序列是 `(2010,0510)`。读写自洽 → 9 个单元测试全绿，但 `print/render.rs:70` 在真实 FilmBox 上拿到空 box 列表 → **渲染出空白胶片**。

### P0-13 C-MOVE 终端响应的子操作计数归零

`server.rs:594-599` C-MOVE 兜底终端响应用 `SubOperationSummary::default()`（全 0），SCP 实际搬完 N 个实例后上报 **0 completed / 0 failed**，`final_status()` 继而返回 SUCCESS 把失败全部掩盖。C-GET（`server.rs:510-513`）是对的，所以这是不一致而非设计取舍。

### P0-14 默认 C-STORE sink 泄漏临时文件 + `max_clients` 是死字段

- `store.rs:46-49` 默认 `SpoolToTemp` 的 `ReceivedInstance{file, path: None}` 使 `write_and_parse` **仅在解析失败时**删除文件（`store.rs:73`）。磁盘验证：一次运行后 `%TEMP%\radicom-store-*.dcm` 从 199 → 200。该 sink 是 `#[default]`，且 `hosting.rs:60-71` 在未配置时选它。
- `max_clients`（`server.rs:33`，默认 0 于 `:84`）在 `run()`（`:125-147`）**完全未使用**，crate 内零 `Semaphore`。实测 `max_clients=1` 时 8 个并发客户端 **8/8 全部成功**，provider 峰值并发 8。
- 服务端无 ACSE 读超时：`association_timeout`（`server.rs:37`，默认 10s 于 `:86`）只在 `server.rs:1059` 的客户端路径被读取。静默对端可无限期占用 task + fd。

---

## 3. 性能问题

所有数字为 release、预热后均值，本机 Windows。

### P-1 全局锁使解析完全不随核数扩展（最严重）

并行解析同一数据集：**1 线程 0.60 ms/次，2 线程 0.42，4 线程 0.27**；同样总工作量的墙钟时间 6.0 → 8.3 → **10.9 ms**，即**吞吐随核数倒退**，4 线程效率约为线性的 13%。

| 根因 | 位置 | 实测 |
| --- | --- | --- |
| 单一全局 `RwLock<HashMap<u32, Arc<Tag>>>`，每元素命中一次 | `model/tag_index.rs:27`（调用点 `reader.rs:281`） | 20.0 ns/次（1 线程）→ **102.8 ns**（4 线程），5.9× 退化 |
| `default_dictionary()` 每次调用都取读锁并 clone `Arc` | `model/dictionary.rs:233-245`（调用点 `reader.rs:195`） | 22.0 ns/次 → **83.3 ns**（4 线程），3.8× |

对 DICOM 服务端意味着：并发关联不能用多核解码，加客户端反而让单客户端延迟变差。

### P-2 文件式解析比内存式慢约一个数量级，根因是每元素 3 次 seek

GH178.dcm（2832 字节 / 65 元素）：`from_bytes` 0.034–0.146 ms，`open(path)` 1.86–2.98 ms（**约 14–53×**，随文件大小而变化），`fs::read` 仅 0.060 ms。

根因：`io/byte_source.rs:66-77` `require(n)` 实现为 `stream_position() → seek(End(0)) → seek(Start(pos))`，**每元素头 3 次系统调用**；且 `io/file.rs:114-170` 用裸 `File` 无 `BufReader`。插桩计数（单次解析 65 元素）：`seek(End)=295`、其他 seek=295、`stream_position=860`，约 **17 次系统调用/元素**。

**正确的修法**是让 `ByteSourceReader` 缓存文件总长度与当前偏移，从而**完全不产生 seek**。分步实测（65 元素）：现状 0.989 ms；只缓存长度、消除 `seek(End)` → 0.577 ms（**+33.7%**）；再缓存位置、零 seek → 0.269 ms（**+69.1%**）。

**注意：加 `BufReader` 不是有效修法，效果随文件反转**——真实解析路径实测 GH178 快 1.58×、ct.dcm 持平 1.02×、mr.dcm **反而慢 0.86×**。控制变量是「每元素的小读次数」：每元素仅约 1 次小读时，缓冲层与自身 seek 记账比省下的 syscall 更贵（−13%）；每元素 4 次小读时它快 142%。因此应修 seek 模式而非加缓冲层。

### P-3 C-STORE 落盘路径约 4 倍吞吐损失 + 双份内存

`radicom-network/src/store.rs` 把入站数据集 spool 到临时文件再重新解析，于是吃上面 P-2 的全部开销；且同步文件 IO 直接跑在 async 任务里（无 `spawn_blocking`）。

### P-4 其它

`read_pdu` 每 PDU 两次分配 + 整帧拷贝（`transport.rs:49-53`）；`write_pdu` 每 PDU 重编码整帧；`SecurityProfile::basic()` 每次构造都重解析 655 行 CSV（**315.7 µs**，而 `anonymize_in_place` 对 500 元素只要 212.8 µs）；`action_for` 每元素线性扫全表；`push_fragment` O(n²) 偏移计算；`dump_dataset` 每层重复取字典。详见各分报告 §3。

### P-5 基准测试看不见以上问题

`crates/radicom/benches/core.rs:98` 只基准 `DicomFile::from_bytes`（内存路径），**从不测 `open()`/`from_reader`**，所以 P-2 的 14–53× 差距在公开基准里完全不可见。

---

## 4. 测试审查（本次最关键结论）

**799 个测试函数全绿，但没有一个能发现上面 14 个高危缺陷中的任何一个。**

| crate / suite | 测试函数数 |
| --- | --- |
| `radicom` lib | 497（默认 feature 372） |
| `radicom` 集成（cli_dump/corpus/fuzz/goldens） | 1 + 5 + 3 + 1 |
| `radicom-dict` | 4 |
| `radicom-json`（lib + goldens） | 34 + 1 |
| `radicom-network`（lib + fuzz/goldens/interop） | 248（默认 239）+ 1 + 1 + 3 |
| `xtask` | **0**（507 行代码） |
| 合计 | **799** |

按判别力分类（`04` §4.1）：有判别力约 45%，只验证「不 panic」约 15%，**自我实现/恒真约 25%**，**与被测实现共享生成逻辑约 15%**。

### 4.1 结构性缺陷：自往返测试无法证明标准符合性

本仓库任何 encode→decode 往返测试只能证明**内部自洽**，永远无法证明符合标准。4 个 A-ASSOCIATE 线格缺陷就是这样活下来的——编码器与解码器共享同一个错误假设，自往返必然通过。同类问题：

| 现象 | 测试状态 | 为什么测不到 |
| --- | --- | --- |
| 渲染在真实文件上返回空数据 | 全绿 | 测试 helper `mono_dataset`（`pixel/mod.rs:556-570`）用 `insert_value(ROWS, /*u32*/)`，存成 `VR=IS` 字符串，走**可用的字符串分支**；真实解析文件是 `VR=US` 原始字节，走**损坏的字节分支**。10 个此类调用点，无一条用例回放真实文件 |
| 10 个文件解析失败/panic | 全绿 | `tests-data/` 只放了 13 个能过的文件；失败的那些**从未进入语料**。`corpus.rs:22-36` 的 `CORPUS` 常量是手挑的 |
| 字符集乱码 | 全绿 | 语料只有 GB2312/ISO-2022-JP；且 `charset.rs:437` 把**错误映射断言成期望值**（断言 `charset_for_encoding(windows-1251) == Some("ISO_IR 110")`） |
| `CodeItem` tag 全错 | 全绿 | `code_item.rs:131` 断言 `SHORT.value == "T-A0100"`，而 `SHORT.value` 就是那个错误 tag——**修好实现反而会让测试变红** |
| JSON group length | 全绿 | golden 由 `xtask/src/main.rs:446-458` 调**被测函数**生成，缺陷被固化成「正确值」 |
| DICOMDIR offset | 全绿 | 测试「扫描自己写出的字节」来验证自己写出的 offset，错误互相抵消 |
| VOI LUT `>>8` | 全绿 | `render/mod.rs:1521-1527` 直接把错误行为写成期望值（断言 `0x8000 → 0x80`，注释「scale by >>8」）；正确用例须用非满量程 LUT（如 `[0,1000]`）检查 255 而非 3 |
| overlay 位序/标签 | 全绿 | `overlay.rs:394-406` 断言自己的赋值（`(0x6002,0x0050)=3` → `origin()==(3,4)`），既不涉行列语义也不覆盖 `ImageFrameOrigin`；`render/mod.rs:804` 与写侧同样自洽错误 |
| print tag 常量 | 全绿 | 9 个单元测试用同一组错误常量读写，自洽通过；`print/render.rs:70` 在真实 FilmBox 上取空列表 |
| C-MOVE 计数 / `max_clients` | 全绿 | 无测试断言子操作计数；`max_clients` 无任何测试，实测 8/8 并发全部放行 |
| C-STORE 临时文件泄漏 | 全绿 | `store.rs:175` 测试名写「removes_the_file」，实际只断言 `path.is_none()` |

### 4.2 文档自评与实测不符

| `docs/development/03-testing.md` 声称 | 实测 |
| --- | --- |
| `:24` 存在 `fuzz/` 目录与 cargo-fuzz 目标 | **`fuzz/` 目录不存在**（`Test-Path fuzz` = False） |
| `:13`/`:69` nightly CI 对 DCMTK、fo-dicom 做外部 interop | 唯一入口 `interop.rs:143` 在未设 `RADICOM_PEER` 时 `eprintln!` 后 `return`（不是 `#[ignore]`），**CI 里永远绿** |
| `:61` byte-level round-trip 与原文件字节相等 | `corpus.rs:60-63` 比较的是**两次 radicom 自己写出的字节**（`first == second`），从未与原文件比较 |
| `:54-59` golden 由 fo-dicom 输出 | `xtask/src/main.rs:446-458` 用 radicom 自己的 `to_dump_string`/`dataset_to_json` 生成 |

### 4.3 值得肯定

环回 TCP 的真实互通测试、PDU golden、DICOMDIR 四层去重、PN 三组结构、InlineBinary base64 等断言是有判别力的；`interop.rs` 的两条真 TCP 环回用例确实在跑。问题不在覆盖率数字，而在**断言的目标是否独立于实现**。

---

## 5. 修复优先级建议

**P0（先修，否则「与 fo-dicom 对齐」不成立）**

1. `width()`/`height()` 改用 `u16`（或让 `read_le_unit` 允许 T 宽于 VR 单元时零扩展），并让 `uncompressed_frame_size()` 对 0 尺寸**报错而非返回 `Ok(0)`** —— 一处修复恢复渲染/图标/MPR/转码四条通路
2. `reader.rs:495` 补消费 delimiter 长度（5 个文件）；`reader.rs:576` 反转 CP-246 条件（GH177）；`dictionary_vr` 改签名收 `&Tag` 保留 creator（P0-9）
3. `charset.rs:195` 溢出（panic），随后把 `BUILTINS` 整表改为 ISO-8859-x/TIS-620
4. A-ASSOCIATE：统一 `id, reserved, result, reserved` 单一布局 + 修 0x54/0x56/0x58 三处线格；**用 golden 字节批注而非自往返测试收口**
5. `dimse.rs:225` 去掉 `expect`，改回 Processing Failure 或 A-ABORT

**P1**：私有序列探测（`IsPrivateSequence`/`IsPrivateSequenceBad`）；`CodeItem` tag 模型重写（`(0008,0100)/(0008,0119)/(0008,0120)` + `(0008,0103)`）；DICOMDIR `item_offsets` 改为写出时直接记录偏移；JSON 剥离 group length；N-service 的 `Requested*` 字段；**VOI LUT 16-bit 重标定**、overlay 三个标签/位序、print 两个 tag 常量（这三组都是"自洽的错"，改完必须配真实数据用例）；C-MOVE 计数、`max_clients`/ACSE 超时。

**P2**：性能三项（tag_index 去锁/分片、`require()` 缓存长度、C-STORE 免落盘）；`MAX_PDU_SIZE` 与协商值解耦；`Suspended`/`IsPartial`。

**P3**：测试体系改造——**这一步优先级实际高于任何单个缺陷**，因为不修它，同类缺陷会以同样方式再次通过：
- 引入 fo-dicom 全量测试数据作为语料，而非手挑 13 个
- golden 必须由**外部 oracle**（fo-dicom 输出或抓包/标准字节）生成，禁止调被测函数
- 给每个协议结构加一条**标准字节 golden**，替代自往返
- 断言目标必须独立于实现：不得把实现常量当期望值（`code_item.rs:131`、`charset.rs:437` 两处必须改）
- 渲染/像素通路必须有「解析真实文件 → 出图」的端到端用例

---

## 6. 与既有自评的差异

| 文档 | 声称 | 实测 |
| --- | --- | --- |
| `docs/notes/01-fo-dicom-alignment-audit.md:18` | 「P0/P1 及 P2 核心功能全部 DONE」 | **14 个高危缺陷**，其中渲染通路在真实文件上完全不可用 |
| `docs/development/02-feature-list.md:167` JSON-01 | 「group length stripped」 | 未剥离，GH178 泄漏 6 个 `*0000` 键 |
| `docs/development/03-testing.md:24` | 有 `fuzz/` 目录 | 目录不存在 |
| `docs/development/03-testing.md:61` | 与原文件字节相等 | 只比较两次自写结果 |
| `docs/development/03-testing.md:13,69` | nightly 外部 interop | 未设环境变量即静默跳过，永远绿 |

**一致的部分**：`anonymizer/ps3.15.csv`（655 行）与 fo-dicom `DicomAnonymizerGenerated.cs` DefaultProfile **完全相同**；`DICOMDictionary.xml.gz` 与 `PrivateDictionary.xml.gz` SHA256 与 fo-dicom **完全一致**；`Tag` 的相等/排序/哈希语义与 `DicomTag.cs:102-177` 一致；匿名化对 `(0008,1140)` 的整条删除与 fo-dicom 一致（PS3.15 表 E.1-1 标 X），**不是缺陷**。这些经差分实测确认，无需返工。
