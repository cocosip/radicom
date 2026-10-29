# 01 — model / io / encoding / validation 审查报告

> 状态提示：本文件是**走查当时的快照**，其中多数“高”缺陷此后已修复。逐条当前处置与证据见
> [`PROGRESS.md`](PROGRESS.md) 与源码/测试；本文正文保留原始结论用于追溯。


审查范围：`crates/radicom/src/model/`、`io/`、`encoding/`、`validation/`、`error.rs`、`logging.rs`、`generated/mod.rs`。
参照实现：`D:\Code\dotnet-source\fo-dicom\FO-DICOM.Core`。
按要求排除：具体图像编解码实现（JPEG / JPEG-LS / J2K / HTJ2K / RLE 的编解码），本文不涉及。

方法说明：结论均来自实际阅读代码 + 实际运行验证。涉及行为争议的条目均用真实 .dcm 文件或最小字节序列实测（探针程序放在系统临时目录，未进仓库）。仓库自带 `docs/*.md` 的完成度声明未被采信——它们正是本次审查对象。

工作区基线由 Lead 预先验证（`cargo build/clippy/test` 全绿，497 + 248 + 34 个测试通过）；本报告不重复验证，且**未修改任何源码**。

---

## 0. 结论摘要

最严重的问题不是"功能少一点"，而是四类**在真实文件上必然出现**的缺陷：

1. **片段序列的 `FFFE,E0DD` 长度字段没被消费**（`reader.rs:495`）：分隔符后只要还有数据，解析器就从错位 4 字节处继续。Lead 提供的 8 个 fo-dicom 能开、radicom 报错的文件中，**5 个是这一个原因**（GH133、GH1986、CR-ModalitySequenceLUT、OutOfBoundsOverlay、D_CLUNIE_CT1_RLE_FRAGS）。
2. **私有序列探测（`IsPrivateSequence` / `IsPrivateSequenceBad`）整体缺失，且私有 tag 从不查私有字典**：GH626 是这一条。
3. **CP-246 的字节序切换条件写反了**：radicom 只在"私有 tag"时切 Little Endian，而标准与 fo-dicom 要求的是"**非**私有 tag"。GH177（Big Endian + UN 定义的 SQ）是这一条。
4. **ISO_IR 字符集整表映射到 Windows 代码页**：Cyrillic / Arabic / Hebrew / Turkish 等全部乱码，且没有任何测试断言这些字符集的输出，所以 CI 全绿。

另有**确定性 panic**（`charset.rs:195` 整数溢出）：debug 构建崩溃、release 构建静默产出错字，而仓库自称 "never panic" 的 fuzz 测试结构上不可能覆盖它。

8 个文件的归因如下（每个都单独复现过）：

| 文件 | 根因 | 章节 |
|---|---|---|
| GH133.dcm | 分隔符长度未消费 | §2.1 |
| GH1986.dcm | 分隔符长度未消费 | §2.1 |
| CR-ModalitySequenceLUT.dcm | 分隔符长度未消费 | §2.1 |
| OutOfBoundsOverlay.dcm | 分隔符长度未消费 | §2.1 |
| D_CLUNIE_CT1_RLE_FRAGS.dcm | 分隔符长度未消费 | §2.1 |
| GH626.dcm | 私有序列探测缺失 + 私有字典未查 | §2.2 / §2.5 |
| GH177_..._BigEndian_undefined_length.dcm | CP-246 字节序条件写反 | §2.5b |
| GH179B.dcm | 缺 `Suspended` / `IsPartial` 语义（fo-dicom 自己就断言该文件 `IsPartial == true`） | §1.1 |

---

## 1. 功能缺失（对照 fo-dicom）

图例：✅ 已实现 ｜ 🟡 部分实现 ｜ ❌ 缺失 ｜ ⛔ 有意不实现

> 判定口径：**以功能是否达成为准**，不要求与 fo-dicom 同名的 .NET 类/接口/结构一一对应。Rust 按其生态
> 惯例实现（具体类型而非 `I*` 接口、标准分配器而非 `ArrayPool` 等）；功能等价即视为已实现，.NET 专有
> 抽象在 Rust 无必要者标“不适用/有意不实现”。

### 1.1 读取器（DicomReader）

| fo-dicom 能力 | 状态 | radicom 对应符号 | 证据 / 行为差异 |
|---|---|---|---|
| `IsPrivateSequence` 探测（私有 SQ 是否真为 SQ） | ✅ | `reader.rs` `private_probe`/`parse_private_sequence` | 私有 SQ 会检查 Item/Delimitation 决定隐式或显式，GH626 类文件正常解析 |
| `IsPrivateSequenceBad`（跳过空 item 后重判隐式/显式） | ✅ | `reader.rs:246` | 跳过空 item 直到有 body 再判定（fo-dicom `IsPrivateSequenceBad`） |
| `handleBadPrivateSequence` 状态贯通（ParseVR/ParseLength/ParseValue 三处） | ✅ | `Step`/`Mode` 的 `private_probe`/`cp246` 标志 | 三处解析共享同一状态位 |
| 私有 tag 走私有字典解析 VR | 🟡（范围取舍） | `reader.rs:208` `dictionary_vr` | radicom 不内置私有字典，未知私有 tag 取 `UN`（隐式 VR 下可正确解析为序列）；标准字典覆盖的 tag 正常 |
| `FFFE,E000` 孤立 item → `DicomTag.Item` 身份（#364 修复） | 🟡 | `reader.rs:605-625` | 结构上还原成了匿名序列，但 tag 用的是 `Tag::UNKNOWN` = `(FFFF,FFFF)`（`reader.rs:609`），fo-dicom 用 `DicomTag.Item` = `(FFFE,E000)`（`DicomReader.cs:583-589`）。实测 GH364.dcm / FreezePattern.dcm 均输出 `(FFFF,FFFF) Sq` |
| 序列中 `FFFE,E00D` 后缺 `FFFE,E0DD`（k-pacs 兼容） | 🟡 | `reader.rs:597-604` | `Step::Item` 遇到 `SequenceEnd` 会向上冒泡（T7），覆盖了部分场景；但缺 `DicomReader.cs:920-933` 里把 `SequenceDelimitationItem` 直接当作 item 结束的显式分支 |
| `Suspended` 结果 + 部分数据集（`IsPartial`） | ✅ | `ReadOutcome::Suspended` / `DicomFile.is_partial` | `is_partial` 由解析结果（非 Success 即部分）置位，截断文件返回部分数据集（GH179B） |
| CP-246：UN 定义的 SQ 在 Big Endian 下强制按 LE 读 | ✅ | `reader.rs` `cp246 = raw_vr == Vr::Un && !tag.is_private()` | 条件与 fo-dicom 一致（非私有 UN 才按 LE），GH177 正确解析 |
| 片段序列第一片段按 4 字节单位换序 | ✅ | `decode_offset_table`（`reader.rs:806`） | 按 `big_endian` 显式以 4 字节为单位解码，尾部非 4 倍数字节记录 debug 后忽略 |
| 序列深度上限 | ✅ | `reader.rs:332-338` | `max_depth`，默认 256 |

### 1.2 写入器（DicomWriter）

| fo-dicom 能力 | 状态 | radicom 对应 | 证据 |
|---|---|---|---|
| `ExplicitLengthSequences` / `ExplicitLengthSequenceItems` | ✅ | `writer.rs:20-28` `WriteOptions` | 私有 SQ 恒用显式长度（`writer.rs:158-171`），与 fo-dicom 一致 |
| `KeepGroupLengths` | ✅ | `writer.rs:20-28` | 重算并注入组长度（`writer.rs:107-124`） |
| `LargeObjectSize`（写入侧大对象分块阈值） | ✅ | `WriteOptions::large_object_size` | 默认 1 MiB，可配置，镜像 fo-dicom 写侧阈值 |
| `DicomWriteOptions` 作为独立可传递对象 | 🟡 | `WriteOptions` | 结构等价，但 `to_bytes`/`save` 每次都要调用方显式传 `&WriteOptions`，无 `Save(string, options)` 重载族 |
| 写出时按目标语法做数值换序 | ✅ | `writer.rs:476-497` | 按 `unit` 分块换序，跨块对齐正确 |
| ACR-NEMA 拒绝保存 | ✅ | `file.rs:40` `is_saveable` | `file.rs:274` 拒绝 |
| File Meta 重新生成 | ✅ | `file.rs:309-334` | 含 `IMPLEMENTATION_CLASS_UID`、`IMPLEMENTATION_VERSION_NAME` |

### 1.3 数据集（Dataset）

| fo-dicom 能力 | 状态 | radicom 对应 | 证据 |
|---|---|---|---|
| `Add` / `AddOrUpdate` / `AddOrUpdate(VR, tag, values)` | 🟡 | `insert` / `insert_or_update` / `insert_value` | `dataset.rs:215`、`:237`、`:246`；缺 fo-dicom 那个"显式指定 VR 覆盖字典"的重载 |
| `AddOrUpdate` 对私有 tag 的 creator 校验 | 🟡 | `resolve_private_write`（`dataset.rs:464`） | 字典式私有 tag 会自动分配 creator block，并把元素/序列/片段重绑定到具体 block；creator block 耗尽时仍受 `insert_or_update` 无错误返回值的 API 限制 |
| `GetPrivateTag(tag, create)` | ✅ | `dataset.rs:316` | |
| `GetCodeItem` / `GetMeasuredValue` / `GetReferencedSOP`（SR 辅助） | ⛔（有意分层） | `sr/code_item.rs` `CodeItem::read_from`、`sr/content_item.rs` `MeasuredValue::read_from`/`ReferencedSop::read_from` | 读取逻辑都在 `sr/` 模块；不在 `Dataset` 上加重导出，避免 `model` 反向依赖 `sr` |
| `FunctionalGroupValues(frame)` | ✅ | `dataset.rs:775` `functional_groups` | 返回临时 Dataset，语义一致 |
| `CopyTo(dest, masked)` | ✅ | `dataset.rs` `copy_to_masked` / `copy_tags` | 支持按 `MaskedTag` 过滤与按 tag 列表复制（顶层，fo-dicom 语义） |
| `Remove(Func<DicomItem,bool>)` | ✅ | `dataset.rs:280` `retain` | 语义等价（谓词签名不同） |
| `DicomDatasetComparer`（可切换 `CompareInstancesByContent`、跳过 BulkDataUri） | ✅ | `dataset.rs` `content_equals` | 内容比较会跳过未解析的 BulkData（`skips_unresolved_bulk`），不再强制物化 `FileWindow`/`BulkDataUri` |
| 私有 tag 排序契约 | 🟡 | `tag.rs` `Ord` | radicom 对私有 tag 按 `element & 0xFF` 再按 creator 排序（`tag.rs:159` 附近有注释承认这是混合公/私排序的折衷）；fo-dicom `CompareTo` 先全 `Element` 后 creator |
| `DicomTag.ToString("G"/"X"/"g"/"J")` | ✅ | `tag.rs` `to_string_with` / `to_json_string` | G/X/g/J 四种格式均实现（X 显示私有 block 掩码） |
| `DicomTag.Parse` 严格性 | 🟡 | `tag.rs` `parse` | radicom 额外接受 `GGGGEEEE` 连写与不足 4 位十六进制（`tag.rs:parse_hex_u16`），fo-dicom 要求 ≥8 字符；radicom 拒绝 `(0028:0010)` 冒号形式 |
| `DicomVM.Parse` 缓存 | 🟡 | `vm.rs:parse` | radicom 每次解析（`vm.rs:50-77`）；fo-dicom 用 `_vm` 字典缓存。仅 `with_generated` 阶段调用，非热路径 |
| `DicomUIDGenerator.RegenerateAll` / `RootUID` / `GetMetaSopClass` | ✅ | `uid.rs` `regenerate_all` / `root_uid` / `meta_sop_class` | 三者均实现并有测试 |
| `StorageCategory` 精确表 | 🟡 | `uid.rs:205` | 用 name-前缀启发式判定，fo-dicom 的生成表是逐条目精确值 |
| `DicomTransferSyntax` 静态表 | ✅ | `transfer_syntax.rs` | 注册表 + `register`/`unregister`，比 fo-dicom 的静态表更灵活 |

### 1.4 文件与缓冲

| fo-dicom 能力 | 状态 | radicom 对应 | 证据 |
|---|---|---|---|
| `FileReadOption.ReadLargeOnDemand` / `SkipLargeTags` / `ReadAll` | ✅ | `file.rs:48-58` | 策略齐全 |
| 惰性窗口（真正的 on-demand） | 🟡 | `buffer.rs` `FileWindow` | `FileWindow` 把整段窗口缓存进 `OnceCell<Bytes>`（`buffer.rs:430` 附近），**首次访问即整体物化**，与"lazy"声明不符；且每个大元素各自 `File::open` 一次，多帧文件有 fd 耗尽风险 |
| `IBulkDataUriByteBuffer` | ✅ | `buffer.rs` `BulkDataUri` | |
| `TempFileBuffer` / `TempFileRemover` | ✅ | `buffer.rs` `TempFileSpill`；`file_ref.rs` `TempFileProvider` | 带 Drop 清扫与延迟重试删除 |
| `CompositeByteBuffer` | ✅ | `buffer.rs` `CompositeBuffer` | 但 `data()` 恒拼接新 Vec（`buffer.rs:225`），`copy_to()` 才流式（`buffer.rs:251`） |
| `EvenLengthBuffer`（读侧自动补齐奇数长度） | 🟡 | `buffer.rs` `EvenLengthBuffer` | 类型存在，但解析路径未使用：`reader.rs:461-511` 的 `parse_fragment_sequence` 接受奇数长度片段而不补/不告警 |
| 内存池 | `ArrayPoolMemory`（.NET `ArrayPool`） | ⛔ 不适用 | `io/pool.rs` `BufferPool`（无生产消费者） | .NET 需 `ArrayPool` 复用托管数组；Rust 用系统分配器 + `Bytes`/`Arc` 共享缓冲，无需另造池。`BufferPool` 既无消费者，属可删除的遗留（代码调整阶段处理） |
| `DirectoryReference` / `IFileReference` | ✅ | `file_ref.rs` | 含 glob、`move_to`、`delete` |

### 1.5 错误与日志

| fo-dicom | 状态 | radicom | 证据 |
|---|---|---|---|
| 强类型异常族 | ⛔ 不适用 | `error.rs` `Error`（core）+ `radicom-network` 自己的 `NetworkError` | .NET 用异常继承族；Rust 惯例是每个 crate 自己的 `thiserror` 枚举，`?` 在边界显式转换。core 的 `Error::Network(String)` 仅承载跨 crate 透传文本，不追求与网络异常同构 |
| 解析器熔断/部分结果 | ✅ | `ReaderError::Eof{position, needed}` / `io_err_need` | 读取路径用 `io_err_need` 记录真实期望字节数；仅定位/跳过等无字节需求的调用用 `needed: 0`（见 §2.8） |
| 日志 target 常量 | ✅ | `logging.rs:5-13` | 与 fo-dicom 命名习惯一致 |

---

## 2. 正确性缺陷

### 2.1 【高】片段序列的 `SequenceDelimitationItem` 长度字段未消费 —— 封装像素数据后必然错位

`crates/radicom/src/io/reader.rs:461-511`（`parse_fragment_sequence`），关键行 **`reader.rs:495`**

```rust
(0xFFFE, 0xE000) => {
    let length = self.source.read_u32().map_err(|e| io_err(e, pos))?;   // :477 —— 读了长度
    ...
}
(0xFFFE, 0xE0DD) => break,                                              // :495 —— 没读长度
```

`FFFE,XXXX` 的 item/delimiter 头都是 **tag(4) + length(4) = 8 字节**。`0xE000` 分支显式 `read_u32()` 取走长度（`reader.rs:477`），但 `0xE0DD` 分支直接 `break`，**把分隔符的 4 字节长度字段留在流里**。返回后 `parse_level` 继续 `step()`，从偏移 = 分隔符 + 4 处读下一个 tag —— 也就是把那个 `00 00 00 00` 长度当成 `(0000,0000)` 元素头，从此全盘错位。

实测最小复现（52 字节，显式 VR LE）：

```
偏移 0..12   : E0 7F 10 00 4F 42 00 00 FF FF FF FF   (7FE0,0010) OB 未定义长度
偏移 12..20  : FE FF 00 E0 00 00 00 00                item: 空 basic offset table
偏移 20..32  : FE FF 00 E0 04 00 00 00 01 02 03 04    item: 4 字节片段
偏移 32..40  : FE FF DD E0 00 00 00 00                SequenceDelimitationItem  ← 长度在 36..40
偏移 40..52  : 08 00 50 00 53 48 04 00 54 41 49 4C    (0008,0050) SH "TAIL"
```

观察到的输出（探针实录）：

```
total=52 bytes
  FRAG   value_start pos=6 (7FE0,0010) OB []
  frag   item_tag pos=20 len=4
  /FRAG  pos=36                       ← 分隔符跨 32..40，但只走到 36
result=Err(Reader(Eof { position: 44, needed: 5242888 }))
```

`/FRAG pos=36` 就是铁证：分隔符结束应在 40，实际停在 36，**差的正好是 4 字节长度字段**。随后 `step()` 在 36 读 tag = `(0000,0000)`（分隔符的长度），`parse_vr` 在 40 读到 `08 00`（非法）走 T2 回退，`dictionary_vr(0,0000)` 判为组长度 `UL`，于是 `parse_length` 把 40..44 的 `08 00 50 00` 当 u32 小端读 → `0x00500008` = **5242888**，与错误里的 `needed` 完全吻合。

**因此：`FFFE,E0DD` 之后只要还有任何字节，解析器必然从错误位置继续。** 反之若分隔符恰是文件最后一个元素（后面无数据），则"碰巧"正确 —— 这正是仓库自测语料（`tests-data/` 仅 13 个简单文件）与 fuzz 测试都测不出来的原因。

对照真实文件（fo-dicom 全部正常打开，radicom 全部 `Reader(Eof ...)`），失败点均紧贴某个 `FFFE,E0DD`：

| 文件 | radicom 失败点（绝对偏移） | 前一个 `FFFE,E0DD` | 分隔符后的残留数据 |
|---|---|---|---|
| `D_CLUNIE_CT1_RLE_FRAGS.dcm` | 254798 | 254786 | `FC FF FC FF 4F 42 00 00 7E 00 00 00 ...`（第二段封装数据） |
| `GH1986.dcm` | 1261874 | 1261858 | `0D CB 10 00 4C 4F 16 00 ...` |
| `CR-ModalitySequenceLUT.dcm` | 1261444 | 1261428 | 同构 |
| `OutOfBoundsOverlay.dcm` | 1020978 | 1020962 | `0D CB 10 00 4C 4F 16 00 ...` |
| `GH133.dcm` | 1081583 | 31030 | 序列内的第二段像素数据（`A1 07 10 00 4C 4F ...`） |

`GH133.dcm` 尤其说明问题性质：分隔符在 31030，但直到 1081583 才报错——中间 105 万字节全部被错位解析（日志里出现 `(0000,0000) UL len=1050529` 这种诡异的"组长度"元素，正是把分隔符长度当 tag 的结果）。

**正确行为**：分隔符的两字段都要消费，流停在 `delimiter_start + 8`。fo-dicom 的 `ParseFragmentSequenceTag` 在命中 `SequenceDelimitationItem` 时 `length = source.GetUInt32()`（`DicomReader.cs:1056`）后才返回 `false`，**长度字段一定被取走**。

**最小修复**（一行）：`reader.rs:495` 改为

```rust
(0xFFFE, 0xE0DD) => { let _ = self.source.read_u32().map_err(|e| io_err(e, pos))?; break }
```

顺手加断言防回归：`debug_assert_eq!(self.position()?, pos + 8);`。

**可加测试**：上述 52 字节序列作为 `parse_dataset_bytes` 的输入，断言解析成功且 `(0008,0050)` 存在。零外部依赖、零文件夹具。

---

### 2.2 【高】私有序列探测逻辑缺失/走反 —— implicit-VR 坏私有序列直接跑飞

`crates/radicom/src/io/reader.rs:431-446`、`reader.rs:192-194`、`reader.rs:222-239`

`GH626.dcm` 的 `(2005,140F)` 处字节（绝对偏移 0x330 附近）：

```
05 20 0F 14 | 00 01 00 00 | FE FF 00 E0 FF FF FF FF
```

VR 字节是 `00 01`（非法），radicom 的 `parse_vr` 落入 T2（`reader.rs:309-312`），回退成隐式并**把长度当 0x00000100 = 256 读**，于是向后走 256 字节垃圾，最终在 1088 处 EOF（实测 `Reader(Eof { position: 1088, needed: 337256453 })`）。而私有字典里 `(2005,140F)` + creator `Philips MR Imaging DD 005` 的 VR 是 **`SQ`**（实测 `vrs=[Sq]`）。

两处根因叠加：

1. **`dictionary_vr` 对私有 tag 直接短路**（`reader.rs:192-194`）：`if tag.is_private() { return if creator_slot { Lo } else { Un } }` —— 私有 tag 的字典 VR **永远拿不到**，即使 creator 已解析、字典里有精确条目。fo-dicom 在 `DicomReader.cs:393-409` 是"查私有字典 → 有多 VR 时取 OW/首个"。
2. **无私有序列探测**：即使 VR 被判成 `SQ`，radicom 也没有 `IsPrivateSequence`（fo-dicom `DicomReader.cs:1099-1122`，探当前位置是否为 Item/SequenceDelimitationItem）与 `IsPrivateSequenceBad`（`DicomReader.cs:1124-1169`，跳过空 item 再判显式/隐式）这两步。radicom 的 `probe_inner_explicit`（`reader.rs:222-239`）只探"item 头之后 offset 12 处是不是合法 VR"，**不跳过空 item**（#223/#487 场景），也不校验 item 头本身。

**正确行为**：implicit/UN 下私有 tag 先查私有字典取 VR；若为 SQ 且长度 > 0，用 `IsPrivateSequence` 确认当前位置确实是 item 或序列分隔符，否则降级为 UN；再用 `IsPrivateSequenceBad` 判定是否需要在子树内切换显式/隐式（并把这个状态一路下传）。

**最小修复**：让 `dictionary_vr` 接收 `&Tag`（含 creator）而不是 `(group, element)`，先查 `default_dictionary().entry(tag)`；当 VR 为 SQ 时执行 item 探测。

**可加测试**：`GH626.dcm` 的关键片段（可用 20 字节抽出）作为 fixture，断言解析后 `(2005,140F)` 是 `Sq` 而非 `UN`。

---

### 2.3 【高】ISO_IR 字符集整表错映射到 Windows 代码页

`crates/radicom/src/encoding/charset.rs:16-50`（`BUILTINS`）对照 `FO-DICOM.Core/DicomEncoding.cs:146-180`（`_knownEncodingNames`）

| 术语 | radicom | fo-dicom | 影响 |
|---|---|---|---|
| ISO_IR 109 | windows-1253 | iso-8859-3 | 拉丁 3 乱码 |
| ISO_IR 110 | windows-1251 | iso-8859-4 | 拉丁 4 乱码 |
| ISO_IR 126 | windows-1253 | iso-8859-7 | 希腊文（部分重合，0xA0-0xFF 区不同） |
| ISO_IR 127 | windows-1256 | iso-8859-6 | 阿拉伯文 |
| ISO_IR 138 | windows-1255 | iso-8859-8 | 希伯来文 |
| ISO_IR 144 | windows-1251 | iso-8859-5 | 西里尔文 |
| ISO_IR 148 | windows-1254 | iso-8859-9 | 土耳其文（encoding_rs 把两者归一，无害） |
| ISO 2022 IR 58 | GBK | gb2312 | 简体中文扩展 |

**字节级证明**（实际运行验证）：

`charset/chrRuss.dcm`，SpecificCharacterSet = `ISO_IR 144`，PatientName 原始字节 `BB EE DA 63 65 DC D1 79 70 D3`：

- windows-1251 → `"»оЪceЬСypУ"` ← radicom 当前输出（实测）
- iso-8859-5 → `"Люкceмбypг"` ← fo-dicom 输出，且被 fo-dicom 自己的测试 `DicomEncodingTest.cs:413` 断言

`charset/chrArab.dcm`，`ISO_IR 127`，字节 `E2 C8 C7 E6 EA 5E E4 E6 D2 C7 D1 20`：

- windows-1256 → `"âباوê^نوزار "` ← radicom 当前输出（实测）
- iso-8859-6 → `"قباني^لنزار "` ← fo-dicom + `DicomEncodingTest.cs:402`

实测 radicom 各文件当前输出：`chrRuss` = `»оЪceЬСypУ`（错）、`chrArab` = `âباوê^نوزار`（错）、`chrGreek` = `Διονυσιος`（对，巧合重合）、`chrHbrw` = `שרון^דבורה`（对，巧合重合）。fo-dicom 期望值见 `DicomEncodingTest.cs:402-416`。

**同时**：`encoding_for_escape`（`charset.rs:244-265`）的 ISO 2022 转义表缺单字节设计符。fo-dicom `DicomEncoding.cs:215-261` 支持 `ESC - A/B/C/D/F/G/H/L/M/T`（Latin1/2/3/4、Greek、Arabic、Hebrew、Cyrillic、Turkish、Thai）与 `ESC $ ) A`（gb2312）。radicom 只认 `(B`、`-A`、`(J`、`(I`、`$B`、`$@(B`、`-C`、`$(D`、`-T`。任何在值中间切到 Cyrillic/Greek/Hebrew/Arabic 的 ISO 2022 文件都会**静默沿用上一段编码**。

**最小修复**：`BUILTINS` 里 7 条改为对应 iso-8859-* 标签；`encoding_for_escape` 补齐 `-A/-B/-C/-D/-F/-G/-H/-L/-M/-T`，并把 `$(D` 保持、`$)C` 走韩国分支。

**可加测试**：直接用上面两组字节断言解码结果，或引入 `chrRuss.dcm`/`chrArab.dcm` 作为夹具。当前**没有任何测试断言这些字符集的输出**——这正是它一直绿的原因。

---

### 2.4 【高】`decode_korean_iso2022` 整数溢出：debug 崩溃、release 静默错字

`crates/radicom/src/encoding/charset.rs:195`

```rust
euc.push(bytes[i] + 0x80); // 7-bit → EUC-KR 8-bit
```

只要在 `ESC $ ) C`（`charset.rs:177` 的 `KO` 常量）之后出现一个 ≥ 0x80 的字节，`u8 + 0x80` 就溢出。

**实测**（5 字节最小复现 `[0x1B,0x24,0x29,0x43,0xFB]` 传给 `decode_multi_charset`，base = EUC-KR）：

```
thread 'main' panicked at crates\radicom\src\encoding\charset.rs:195:22:
attempt to add with overflow
```

- debug 构建：**panic**，整个 `DicomFile::open` 崩溃。
- release 构建：`0xFB + 0x80` 回绕成 `0x7B` = `'{'`，**静默产出错误文本**，无任何错误返回。

真实文件实测：`charset/chrI2.dcm` 与 `charset/chrKoreanMulti.dcm` 均 panic（`catch_unwind` 捕获确认），而 fo-dicom 分别应得 `"Hong^Gildong=洪^吉洞=홍^길동"` 与 `"김희중"`（`DicomEncodingTest.cs:409,412`）。

**正确行为**：7 位模式下的韩文字节本应在 0x21-0x7E 范围；遇到 ≥ 0x80 说明流里已是 8 位形式（混合编码），应当**原样传递而非加 0x80**。fo-dicom 用 `x-cp20949`（`DicomEncoding.cs:158`）交给 .NET 处理，不会手工平移字节。

**最小修复**：`euc.push(if bytes[i] < 0x80 { bytes[i] + 0x80 } else { bytes[i] });` —— 一行。

**为什么 fuzz 测试没抓到**：`crates/radicom/tests/fuzz.rs:4` 声称"never panic or hang"，但 `arbitrary_bytes_never_panic`（`fuzz.rs:39-55`）用随机字节生成 ≤300 字节输入，随机命中 `ESC $ ) C` 四字节设计符且后随 ≥0x80 字节的概率极低；`mutated_valid_file_never_panics`（`fuzz.rs:58-70`）只翻转 GH487.dcm（非韩文文件）的 1-4 个 bit。二者都触及不到该分支。**该 doc comment 的承诺与实现不符。**

**可加测试**：断言 `decode_multi_charset(&[0x1B,0x24,0x29,0x43,0xFB], EUC_KR, false)` 不 panic 且返回确定性字符串。5 字节，零成本。

---

### 2.5 【高】私有 tag 的 VR 恒为 UN —— 隐式/UN 文件里所有私有元素的类型访问失效

`crates/radicom/src/io/reader.rs:187-207`（`dictionary_vr`），关键行 `reader.rs:192-194`

`dictionary_vr` 对任何私有 tag（非 creator slot）无条件返回 `Vr::Un`，从不查私有字典。后果：**隐式 VR 文件或 UN 编码文件里的每一个私有元素都以 `UN` 落地**，`get::<i32>()`、`first::<u16>()` 等类型化读取全部退化为字节视图。

实测（`default_dictionary().entry()` 直接查询，证明字典数据本身是好的）：

```
(0009,1001) +GEMS_IDEN_01   vrs=[Lo]
(0011,1010) +GEMS_PATI_01   vrs=[Ss]
(0019,1002) +GEMS_ACQU_01   vrs=[Sl]
(2005,140F) +Philips MR Imaging DD 005  vrs=[Sq]
(2005,140B) +Philips MR Imaging DD 005  vrs=[Lo]
```

但解析器在隐式语法下给出的都是 `UN`。这解释了 Lead 观察到的 "ct.dcm / mr.dcm / test_1847.dcm 上约 40 个私有元素 VR 错误"。

**正确行为**：fo-dicom `DicomReader.cs:393-409`（隐式）与 `551-557`（UN）都查 `_dictionary[tag]`，其中 `tag` 已带 creator（`DicomReader.cs:332-335`）。radicom 的 `resolve_tag`（`reader.rs:280-293`）确实给 tag 绑定了 creator，但 `dictionary_vr` 收的是 `(group, element)` 两个 `u16`，**把 creator 信息丢了**。

**最小修复**：`dictionary_vr(&self, tag: &Tag)` 签名改为接收已解析的 `Tag`，走 `default_dictionary().entry(tag)`，仅在字典条目为 Unknown 时回退 UN。

**可加测试**：构造一个隐式 LE 的 `(0009,1001)` 私有元素 + `(0009,0010)` creator `GEMS_IDEN_01`，断言解析后 VR 为 `LO`。

### 2.5b 【高】CP-246 字节序切换条件与标准相反 —— Big Endian + UN 定义的 SQ 必然读错长度

`crates/radicom/src/io/reader.rs:573-596`，关键行 `reader.rs:576-581`；触发条件由 `reader.rs:431-436` 决定

CP-246 规定：**显式 VR 数据集中以 `UN` 编码的未定义长度元素**必须按 Implicit VR Little Endian 解读其内容（无论数据集本身的字节序）。fo-dicom 的判定是：

```csharp
// DicomReader.cs:608-616
var needtoChangeEndian = false;
if (parsedVR == DicomVR.UN && !tag.IsPrivate)   // ← 非私有
    needtoChangeEndian = source.Endian == Endian.Big;
if (needtoChangeEndian) source.Endian = Endian.Little;
```

radicom 的切换只在 `private_probe == true` 时发生（`reader.rs:576`），而 `private_probe` 仅在 `tag.is_private()` 且长度未定义时置真（`reader.rs:440-443`）。相反地，非私有的 UN 未定义长度元素走 `reader.rs:431-434` 的 `is_sequence` 分支，`private_probe` 被硬编码为 `false`（`reader.rs:436`），**永远不会切到 Little Endian**。

实测（`GH177_D_CLUNIE_CT1_IVRLE_BigEndian_undefined_length.dcm`，语法 `1.2.840.10008.1.2.2`，Big Endian）：

```
  elem abs=     820 d=0 (0008,2111) ST len=50
  SEQ  abs=     884 d=0 (0008,2112) defined=None      ← UN + 未定义长度 + 非私有
  result=Err(Reader(Eof { position: 542, needed: 1476395008 }))
  Eof at abs=898 needed=1476395008 (0x58000000)
  0000036A  31 31 31 33 00 08 21 12
  00000372  55 4E 00 00 FF FF FF FF      ← (0008,2112) UN, 未定义长度
  0000037A  FE FF 00 E0 58 00 00 00      ← item, 长度字节 58 00 00 00
  00000382  08 00 50 11 1A 00 00 00      ← 内部元素 (0008,5011) 小端编码
```

`needed = 1476395008 = 0x58000000` 正好是 item 长度字段 `58 00 00 00` **按大端**解读的结果；按 CP-246 应为小端 `0x00000058 = 88`。内部元素 `(0008,5011)` 的 `1A 00 00 00` 也证明内容确实是小端。**这一条把根因钉死了。**

**正确行为**：切换条件是 `parsed_vr == UN && !tag.is_private()`，与 `private_probe` 无关。

**最小修复**：`Step::Sequence` 增加一个 `cp246_endian_switch: bool` 字段（或直接判定 `raw_vr == Vr::Un && !tag.is_private()`），在 `reader.rs:576-581` 一并处理。

**可加测试**：构造 Big Endian 的 `(0008,2112) UN` 未定义长度 + 一个 88 字节 item（内部小端元素），断言解析成功。若不便构造 BE 输入，直接用 `GH177` 文件作夹具（530KB，可接受）。

### 2.5c 【中】缺 `Suspended` / `IsPartial` 语义 —— 截断文件整体失败而非部分成功

`crates/radicom/src/io/reader.rs:104-110`（`ReadOutcome` 只有 `Success`/`Stopped`）、`crates/radicom/src/io/file.rs:187`、`file.rs:234`（`is_partial` 恒 `false`）

fo-dicom 有三个结果态：`Success` / `Stopped` / `Suspended`，且 `DicomFile.cs:250` 明确：

```csharp
df.IsPartial = result == DicomReaderResult.Stopped || result == DicomReaderResult.Suspended;
```

radicom 没有 `Suspended`：`byte_source.rs:66-77` 的 `require` 失败一律返回 `ReaderError::Eof`，`reader.rs:362` 把"头部不够 4 字节"转成 `LevelEnd::Eof`（优雅结束），但**任何值长度不足都直接冒泡成 `Err`**。

`GH179B.dcm` 正是这个场景。实测：解析到 `(0040,0275)` 的 SQ（偏移 1480，`FF FF FF FF` 未定义长度）后，item 长度 `08 01 00 00` = 264，内部元素一路读到文件尾（文件仅 2848 字节），最后在 `needed: 1330384980` 处失败。fo-dicom 对同一文件的测试是：

```csharp
// Tests/FO-DICOM.Tests/Bugs/GH179.cs:24-29
public void DicomFile_Open_PartialDatasetAvailable(string fileName) {
    var file = DicomFile.Open(TestData.Resolve(fileName));
    Assert.True(file.Dataset != null);
    Assert.True(file.IsPartial);      // ← 明确断言这是"部分成功"
}
```

即 fo-dicom 认为该文件**本来就是截断的**，应当返回已解析的部分数据集并标 `IsPartial = true`；radicom 直接报错，调用方拿不到任何数据。

**注意**：radicom 的 `ReadOutcome::Stopped` 只服务于 `stop` 谓词（`reader.rs:398-408`），与"输入提前结束"是两件事。要补齐需引入 `Suspended` 语义：`require` 失败路径改为返回"已成功解析 N 个元素"的部分结果，而非 `Err`。

**可加测试**：`GH179B.dcm` 作夹具，断言 `open()` 成功、`is_partial == true`、且已解析元素数 > 0。

---

### 2.6 【中】孤立 item 序列的 tag 身份是 `(FFFF,FFFF)` 而非 `(FFFE,E000)`

`crates/radicom/src/io/reader.rs:605-625`，关键行 `reader.rs:609`：`let unknown = Tag::UNKNOWN;`

实测 `GH364.dcm` 与 `FreezePattern.dcm` 均产生 `(FFFF,FFFF) Sq`；fo-dicom 用 `DicomTag.Item` = `(FFFE,E000)`（`DicomReader.cs:583-589`：`tag.Equals(DicomTag.Item)` 时回退到 item 位置并按 SQ 处理）。

影响：dump / JSON 输出出现伪造 tag；任何按 `(FFFE,E000)` 查找的逻辑失效；也违反 PS3.5 §7.5（`FFFE` 组是 item 专用）。

**最小修复**：新增 `Tag::ITEM = Tag::new(0xFFFE, 0xE000)` 常量，`reader.rs:609` 改用它。

**可加测试**：`GH364.dcm` 作为夹具，或构造"文件开头即 `FFFE,E000`"的 16 字节输入。

---

### 2.7 【中】`ReadLargeOnDemand` 首次访问即整体物化，且每元素一个 fd

`crates/radicom/src/model/buffer.rs:430` 附近（`FileWindow` 的 `OnceCell<Bytes>` cache）

`FileWindow` 名字与文档都暗示"惰性窗口"，但 `data()` 一旦被调用就 `cache.set(整段 Bytes)`，之后永不释放。因此：

- `ReadLargeOnDemand` 对大文件**并不省内存峰值**，只是把分配推迟到第一次 `data()`。
- `Dataset::eq`（`dataset.rs:751` + `element.rs:467` 的 `a.data() == b.data()`）会对两侧所有大值**强制物化**。
- `FileWindow::open` 每个大元素各自打开一次文件句柄；多帧（数百帧）文件会同时持有数百 fd。

**正确行为**：fo-dicom 的 `FileByteBuffer` 只在 `GetByteRange` 时做区间读，不缓存整段。

**最小修复**：`FileWindow` 改为持有已打开的 `File` + 偏移/长度，`data()` 用 `get_byte_range` 分块读取；或至少在 `ReadLargeOnDemand` 路径上改为 `RangeBuffer` 语义。**注意**：`file.rs:112` 的 doc comment 声称"large values stay as lazy FileWindows instead of being copied into memory"，与实现不符，属文档与实现不一致。

**测试**：需要 `ReadLargeOnDemand` 下读取一个大值两次并观察内存/句柄数——现有测试（`file.rs:947` `skip_large_tags_leaves_a_placeholder_and_advances`）只测 `SkipLargeTags`，不覆盖该路径。

---

### 2.8 【中】`io_err` 丢弃真实所需字节数

`crates/radicom/src/io/reader.rs:514-518`，关键行 `reader.rs:516`：

```rust
std::io::ErrorKind::UnexpectedEof => Error::Reader(ReaderError::Eof { position, needed: 0 }),
```

对比 `require()` 抛出的 `ReaderError::Eof { position, needed: n }`（`byte_source.rs:74`）—— 那条路径是正确的。但所有走 `read_exact` 的路径（`read_tag`、`read_u16/u32`、`read_value_buffer` 等）都会把 `needed` 抹成 0。

实测证据：`GH626.dcm` 的错误是 `Eof { position: 1088, needed: 337256453 }` —— 这里的 needed 之所以非 0，是巧合走了 `require()`；而 Lead 报告的其它文件 `needed: 1100557` 之类也是 `require` 路径。一旦走到 `read_exact`，诊断信息就退化成 0。

**最小修复**：无法从 `io::Error` 反推所需字节数，应在调用点把期望长度传给 `io_err`（`io_err_need(e, pos, need)`），或在 `ByteSourceReader` 内统一用 `require` + `read_exact` 组合。

### 2.9 【已修复，保留 API 限制】`insert_or_update` 对未解析的私有 tag 静默丢弃

`crates/radicom/src/model/dataset.rs:237-243`

```rust
pub fn insert_or_update(&mut self, item: impl Into<DicomItem>) -> &mut Self {
    let item: DicomItem = item.into();
    if let Some(t) = self.resolve_private_write(item.tag().clone(), true) {
        self.items.insert(t.clone(), item.with_tag(t));
    }
    self
}
```

现在空闲 creator block 存在时会先分配 creator 元素，再将传入 item 的 tag 一起绑定到实际 block，避免只更新 `BTreeMap` key 而在序列化时仍写出字典式 element。回归测试 `insert_or_update_resolves_and_retags_new_private_creator` 覆盖了该路径。

当前方法保持 `&mut Self` 返回值；当 240 个 creator block 全部占用时仍无法通过该 API 报告失败，调用方可先用 `get_private_tag(tag, true)` 检查容量，或使用返回 `Result` 的 `insert`。

### 2.10 【中】校验规则与 fo-dicom / PS3.3 的契约差异

`crates/radicom/src/validation/mod.rs`

| 规则 | radicom | fo-dicom (`DicomValidation.cs`) | 判定 |
|---|---|---|---|
| DA | 接受 `YYYY`、`YYYYMM`、`YYYY.MM.DD`、`YYYY/MM/DD` | 严格要求 8 位 `YYYYMMDD` | radicom **过宽** |
| DT 负偏移 | `has_invalid_dt_offset` 限制 ≤12h / ≤14h | 无此检查 | radicom **额外限制**（PS3.3 未规定上限，属自创规则） |
| IS | 接受 `3.0`（截断小数） | 正则 `^[+-]?\d+$` 拒绝 | radicom **过宽** |
| DS | 接受 `,` 作小数点（`parse_ds` 容错） | 只接受 `.` | radicom **过宽**（DICOM 十进制用 `.`） |
| AE | 允许内部空格 | 允许（≤16 字符） | 一致 |
| CS | `[A-Z0-9_ ]`（允许内部空格） | 一致 | 一致 |

另外 `validation/mod.rs:50` 的 `fail()` 丢弃了 `_vr`/`_content` 两个入参，`:56` 有未使用的 `empty_ok` 变量 —— 说明原本打算把 VR 与原始内容写进错误消息但没做完，错误信息对排障不足。

### 2.11 【低】`Tag::parse` 与排序契约偏离

`crates/radicom/src/model/tag.rs`

- `parse` 接受不足 4 位的十六进制（`parse_hex_u16`），如 `(28,10)`；fo-dicom 的 `DicomTag.Parse` 要求 ≥8 字符。宽容对读取有利、对写入侧 round-trip 有害。
- 私有 tag 的 `Ord` 用 `element & 0xFF` 参与比较（`tag.rs:159` 附近注释自承是 hack），导致同一 group 下公私混合时排序与 fo-dicom 不同。`BTreeMap` 的迭代顺序因此与 fo-dicom 的 `Dictionary` 不一致，dump 对比会假阳性。

### 2.12 【低】`push_fragment` 的 O(n²) 偏移计算

`crates/radicom/src/model/item.rs:102-110`

```rust
pub fn push_fragment(&mut self, fragment: SharedBuffer) {
    let start: u64 = self.fragments.iter().map(|f| 8 + f.size() + f.size() % 2).sum();
```

每次 push 都重新累加全部已有片段，n 个片段总代价 O(n²)。1000 帧的多帧文件在白造 offset table 时约 50 万次加法。

更糟的是：解析路径上 `dataset_builder` 的 `on_end_fragment` 会用**线上真实的 offset table 覆盖**这里算出来的值，所以解析时的这 O(n²) 全是无用功。

**最小修复**：加一个 `running_offset: u64` 字段增量累加；或解析路径改用 `push_raw`（不维护 offset table）。

### 2.13 【低】`charset_encoding` 每次调用都分配 + 加锁

`crates/radicom/src/encoding/charset.rs:83-90`：`normalize_term` 每次 `collect()` 出新 `String`，然后 `REGISTRY.lock()`。解析时每层 charset 只在切换时调用（`dataset_builder.rs:67-83` 有缓存），但写入路径按元素调用。热路径上可改为 `&'static Encoding` 缓存或 `HashMap<&str, _>` 无分配查找。

---

## 3. 性能问题

### 3.1 【已测量】`require()` 每次 3 次 syscall + 无缓冲 → 磁盘解析比内存慢 10-50 倍

`crates/radicom/src/io/byte_source.rs:66-77`（`require`）、`byte_source.rs:79-84`（`is_eof`）、`crates/radicom/src/io/file.rs:114-170`（`open_with_options` 未包 `BufReader`）

`require(n)` 的实现是：`stream_position()` → `seek(End(0))` → `seek(Start(pos))`，**每个元素头都做一遍**。

我的实测（GH178.dcm，2832 字节，65 元素，release，300 次迭代，warm up 20 次）：

```
from_bytes   = 0.0560 ms      (内存 Cursor)
from_reader  = 0.0383 ms      (Cursor —— 同样的解析逻辑，无磁盘)
open(path)   = 2.9750 ms      (真实 File)
fs::read     = 0.0275 ms
ratio open/from_bytes = 53.1x
```

与 Lead 的 14x 量级差异来自机器与迭代次数不同，但**结论一致：磁盘路径的开销 ~99% 在 IO 管线而非解析**。`from_reader`（0.0383ms，Cursor）甚至比 `from_bytes` 更快，说明 `from_bytes` 的内部拷贝也有成本。

**syscall 计数**（用计数包装器实测单次解析 GH178.dcm）：

```
seek(End) = 282    seek(other) = 282
stream_position = 826    read() calls = 738
→ 65 个元素，平均每个元素 4.3 次 seek(End) + 12.7 次 stream_position + 11.4 次 read
```

`require` 被调用的位置：`reader.rs:362`（每个元素头）、`reader.rs:470`（每个片段）、`reader.rs:225`（T4 探测）、`reader.rs:248`（每个值）。加上 `reader.rs:360` 每元素一次 `self.position()`（`stream_position`），总计每个元素约 17 次 syscall。

**最小修复（正确的层）**：`ByteSourceReader` 增加 `known_len: Option<u64>` 字段，构造时（`file.rs:160`、`file.rs:134` 已知 `file_len`）设置一次，则 `require`/`is_eof` 变成纯算术：

```rust
pub fn require(&mut self, n: u64) -> Result<(), ReaderError> {
    if let Some(total) = self.known_len {
        let pos = self.cached_pos;              // 由 read/seek 维护
        if total.saturating_sub(pos) < n { return Err(ReaderError::Eof { position: pos, needed: n }); }
        return Ok(());
    }
    /* 旧的 seek 路径作为 fallback */
}
```

同时维护一个 `cached_pos` 由所有 `read_*`/`seek`/`skip` 更新，消掉 `stream_position`。

**band-aid**：`file.rs:160` 处把 `let mut source = ByteSourceReader::new(file, syntax.endian())` 改成 `ByteSourceReader::new(BufReader::with_capacity(64*1024, file), syntax.endian())`。Lead 实测 `raw File 1.88ms → BufReader(64K) 1.12ms`（约 40%），但纯内存是 0.15ms —— 所以缓冲只是创可贴，`require` 的 seek 模式才是主因。

**`from_bytes` 不需要改。**

### 3.2 【已测量】发布的 benchmark 完全测不到慢路径

`crates/radicom/benches/core.rs:98`

bench 只测 `DicomFile::from_bytes(black_box(&encoded), ...)`（`core.rs:98`）和 `to_bytes`，即**纯内存路径**。`DicomFile::open` / `from_reader` 从未被基准覆盖。因此"benchmarks published"（`core.rs:1` 的注释）会给出 0.056ms 的数字，而真实用户 `open()` 一个 2.8KB 文件要 2.97ms —— **差距 53 倍，基准完全看不见**。

这是 benchmark 覆盖缺口，建议在 `core.rs` 增加 `open(path)` 与 `from_reader(BufReader<File>)` 两行。

### 3.3 【明显】`CompositeBuffer::data()` 恒拼接新 Vec

`crates/radicom/src/model/buffer.rs:225`

`data()` 把所有子 buffer 拷贝进一个新 Vec；`copy_to()`（`buffer.rs:251`）才是流式的。任何调用 `data()` 的地方（包括 `element.rs:467` 的相等比较、`element.rs:167` 的 `select`）都会产生整段拷贝。大像素数据上这是 O(size) 额外分配。

**建议**：审计 `data()` 调用点，除确有必要的（如需连续切片），一律改 `copy_to` / `get_byte_range`。

### 3.4 【明显】`dictionary::entry` 每次私有查找都新建 `Arc` + 线性扫描

`crates/radicom/src/model/dictionary.rs:177`（public masked 的 `Vec` 线性 `.find`）、`:163`（private-creator 槽位每次 `Arc::new` 一个 placeholder 条目）

`entry()` 在解析热路径上被**每个元素调用一次**（经 `reader.rs:195` → `dictionary_vr`）。私有 tag 的 masked 查找是 `Vec<Arc<DictionaryEntry>>` 线性扫描；creator 槽位条目每次调用都新分配。

**建议**：私有 masked 表改为按 `u32` 前缀分桶的 `HashMap`；creator 槽位 placeholder 缓存为 `Lazy` 常量。

### 3.5 【明显】`default_dictionary()` 每次调用都拿一次 `RwLock`

`crates/radicom/src/model/dictionary.rs:233-245`

```rust
pub fn default_dictionary() -> Arc<Dictionary> {
    if let Some(d) = DEFAULT.read().expect("default lock").clone() { return d; }
    GENERATED.clone()
}
```

每次调用一次 `RwLock::read()` + `Arc::clone`。同上，每元素一次。`set_default_dictionary` 是一次性守卫（`dictionary.rs:249-258`），可以用 `OnceLock`/`AtomicPtr` 把 fast path 变成无锁。

### 3.6 【明显】`Uid::enumerate` 每次排序

`crates/radicom/src/model/uid.rs:191-196`：每次调用都 `sort` 一遍注册表。若无调用方依赖有序性，改为返回迭代器；若依赖，用 `Lazy` 缓存排序结果。

### 3.7 【明显】`read_meta_block` 反复从头重读

`crates/radicom/src/io/file.rs:414-427`：以 `take * 2` 增长并从 `start` 重新 seek+read。对超大 meta 组（少见但可能，如带巨大私有 meta）是 O(k²) 重读。有界，低优先。

### 3.8 【明显】deflate 路径双份峰值内存

`crates/radicom/src/io/file.rs:151-157`：先把整个剩余文件读进 `comp`，再 `parse_dataset_with(&comp, ...)` 内部 inflate 成另一个 Vec。峰值 = 压缩 + 解压 ≈ 2 倍。可用流式 `ZlibDecoder` 直接喂给解析器（解析器已接受 `Read+Seek`，只需包一层）。

### 3.9 【可疑，未测量】`decode_multi_charset` 对纯 ASCII 无快路径

`crates/radicom/src/encoding/charset.rs:120-171`：即使值完全是 ASCII（绝大多数 DICOM 文件的所有 PN/LO/SH），也要逐字节扫描并在最后 `flush` 时走一次 `enc.decode`。scan 本身不贵，但 `encoding_rs::ASCII.decode` 也会做校验。

**建议**：入口加 `if bytes.is_ascii() { return String::from_utf8_unchecked(bytes.to_vec()) }`（或 `from_utf8` + `expect`，ASCII 必合法）。J2K/JPEG 路径的性能占比更高，此项优先级低于 3.1。

### 3.10 【可疑，未测量】`private_creators` 用 `HashMap<(u16,u8), String>`

`crates/radicom/src/io/reader.rs:154` 附近的 `PrivateCreators` 类型。每个私有元素都要 `get(&(group, block))`，SipHash 开销对短 key 不划算。可换 `[Option<String>; 256]` per-group 或 `Vec`。非瓶颈，低优先。

---

## 4. 测试审查

### 4.1 同义反复（断言在自己的构造上）

| 测试 | 位置 | 问题 |
|---|---|---|
| `unknown_tags_are_fresh` | `crates/radicom/src/model/tag_index.rs:56`（断言在 `:61`） | `assert_eq!(len(), lookup_interned(...).map(\|_\| len()).unwrap_or(len()))` —— 两侧都是 `len()` 的同一表达式，无论实现如何都成立。有效的断言应比较两次 `len()` 前后不变，或比较 `Arc::ptr_eq`。 |
| `write_to_log_does_not_panic` | `crates/radicom/src/logging.rs:88` | 函数体只有一次调用，**零断言**。用空实现 `fn write_to_log(&self, _: LogLevel) {}` 也能通过。有效版本应安装一个 `tracing` subscriber 并断言收到事件（`io/trace.rs:176` 就是这么做的，可复用该模式）。 |
| `internal_vrs` | `crates/radicom/src/model/vr.rs:395` | 断言 `unit_size() == 0`（`Vr::Item`/`ItemDelim`/`SeqDelim`/`Implicit`）。这是实现的自我定义，不是外部契约。 |
| `encode_replacement_fallback` | `crates/radicom/src/encoding/charset.rs:486` | 只断言 `text.contains("caf")`（`:491`）。任何把 `é` 变成 "?"、"X" 甚至删掉的实现都通过。应断言替换字符的确切值（U+FFFD 或 `?`，取决于契约）。 |
| `the_shared_pool_lends_and_reclaims` | `crates/radicom/src/io/pool.rs`（`:199` 附近） | 只断言 `len()`，不验证归还后内容、容量上限、跨线程安全性。 |

### 4.2 假绿（用 stub 也能过 / 只测了 happy path）

| 测试 | 位置 | 问题 |
|---|---|---|
| `declared_length_beyond_the_file_errors_without_allocating` | `crates/radicom/src/io/dataset_builder.rs:726` | 只断言 `is_err()`，不检查错误种类与 `position`/`needed`。任何错误都通过——包括"解析器读错位置导致的 EOF"（即 §2.1 的 bug 形状）。 |
| `charset_is_used_for_encoding` | `crates/radicom/src/io/writer.rs:727` | 写入与读回用**同一套** charset 代码。若 `BUILTINS` 整表错映射（§2.3 已证实），写用 windows-1252、读也用 windows-1252，round-trip 依然相等。**这正是 §2.3 一直绿的原因。** |
| `decode_with_escape_switching` | `crates/radicom/src/encoding/charset.rs:447` | 只断言 `out.contains("AB")` 与 `out.contains("CD")`，不检查 ESC 段实际用哪个编码解码。`ESC - A` 走错编码也通过。 |
| `decode_pn_resets_at_caret` | `crates/radicom/src/encoding/charset.rs:459` | 只断言 `starts_with("Doe^")`，不检查 `José` 是否正确。 |
| `save_and_reopen_round_trip` 系列 | `crates/radicom/src/io/file.rs:630`、`:840` 等 | 全是"写出去再读回来"，自证自洽。`GH626`/`GH133` 这类真实世界畸形文件从未进入语料（`tests-data/` 只有 13 个简单文件）。 |
| `fuzz.rs` 全部 | `crates/radicom/tests/fuzz.rs:39`、`:58` | doc comment（`:4`）承诺 "never panic"，实际漏掉 §2.4 的韩文溢出路径（随机字节几乎不可能构造 `ESC $ ) C` 后随 8 位字节）。承诺与覆盖不符。 |

### 4.3 未覆盖的文档行为

| 行为 | 相关代码 | 缺什么测试 |
|---|---|---|
| 私有序列探测 | `reader.rs:431-446` | 无任何私有 SQ + 坏 VR 的测试。`GH626.dcm` 可直接作为夹具。 |
| 分隔符后仍有数据 | `reader.rs:495` | 无。§2.1 的 52 字节夹具可零成本加入。 |
| 隐式 VR 下的私有字典解析 | `reader.rs:192-194` | 无。构造 2 个元素即可测。 |
| ISO_IR 编码正确性 | `charset.rs` 映射表 | 已覆盖：`charset.rs` 断言 Iso-8859 系列往返，`corpus.rs` 用 `chrRuss`/`chrArab`/`chrGreek`/`chrHbrw` 断言真实解码文本 |
| ISO 2022 单字节设计符切换 | `charset.rs:244-265` | 无。缺 `ESC - L`（Cyrillic）等设计符导致静默沿用旧编码。 |
| `ReadLargeOnDemand` 的真惰性 | `buffer.rs:430` | `file.rs:947` 只测 `SkipLargeTags`。 |
| `insert_or_update` 的私有 tag 静默丢弃 | `dataset.rs:222` | 无。 |
| `DicomFile::open` 性能 | `benches/core.rs:98` | 基准只测 `from_bytes`（见 §3.2）。 |

---

## 5. 优先修复顺序建议

| 优先级 | 项 | 理由 |
|---|---|---|
| P0 | §2.4 charset 溢出（一行） | 确定 panic，release 静默错字 |
| P0 | §2.1 分隔符双消费 | 8 个真实文件里 5 个的唯一根因 |
| P0 | §2.3 ISO_IR 整表 | 7 个字符集全错，用户可见 |
| P1 | §2.5 + §2.2 私有字典 VR / 私有序列探测 | 同一个函数域，一起修；影响所有隐式 VR 私有元素 |
| P1 | §3.1 `require()` syscall 风暴 | 53x，所有磁盘读写路径受益 |
| P2 | §2.6 孤立 item tag、§2.7 FileWindow 惰性、§2.8 needed 丢失 | 正确性/契约 |
| P3 | §3.3–3.8 缓冲与字典热路径 | 优化，需先有 benchmark 覆盖 |

**测试策略**：以上 P0/P1 全部可用 ≤20 字节的最小编码夹具覆盖，无需引入外部 .dcm 文件。§2.3/§2.4 更是只差两行断言。
