# 序列化 / 介质 / SR / 匿名化 评审，以及全 workspace 测试质量审计

> 状态提示：本文件是**走查当时的快照**，其中多数“高”缺陷此后已修复。逐条当前处置与证据见
> [`PROGRESS.md`](PROGRESS.md) 与源码/测试；本文正文保留原始结论用于追溯。


评审对象：

- `crates/radicom/src/media/{mod,scanner,icon}.rs`（DICOMDIR、扫描器、缩略图）
- `crates/radicom/src/sr/{mod,code_item,content_item,structured_report}.rs`
- `crates/radicom/src/anonymizer/{mod,profile}.rs` + `ps3.15.csv`
- `crates/radicom/src/rules/{match_rule,transform}.rs`
- `crates/radicom/src/dump/mod.rs`、`crates/radicom/src/bin/radicom-dump.rs`、`crates/radicom/src/logging.rs`
- `crates/radicom-json/src/{lib,xml}.rs`、`crates/radicom-dict/src/lib.rs`、`xtask/src/main.rs`
- **全 workspace 测试**：`crates/*/tests/*.rs` 全部 16 个测试函数 + `crates/*/src/**` 全部 86 个内联 `#[cfg(test)]` 模块（783 个测试函数），合计 **799** 个测试函数

参照实现：fo-dicom `FO-DICOM.Core/` 下的 `Media/DicomDirectory.cs`、`Media/DicomDirectoryReaderObserver.cs`、`Media/DicomFileScanner.cs`、`Media/DicomIconImageSequenceBuilder.cs`、`StructuredReport/*.cs`、`DicomAnonymizer.cs`、`DicomMatchRules.cs`、`DicomTransformRules.cs`、`Serialization/{DicomJson,DicomXML,JsonDicomConverter}.cs`、`Log/DicomDatasetDumper.cs`、`Network/DicomAssociation.cs`；规范 PS3.3 Appendix F、PS3.10、PS3.15、PS3.18 Annex F、PS3.8。

方法：逐行读实际代码与测试；对每个结论都找到可达调用点。所有"缺失"结论均经 grep 确认 repo 内无对应符号。**不采信 `docs/*.md` 的自评**——本文第 4 节直接抽查并推翻了其中至少 4 处自评。当需要证明"测试是否为假绿"时，在系统临时目录复制一份 workspace 打桩验证（见 §4.3 各条），**repo 内未修改任何文件**（`git status --porcelain` 为空）。

**基线**：`cargo build/test/clippy --workspace --all-features` 全绿（Lead 已统一验证）；本文复现的计数为 `cargo test --workspace`（默认 feature）372 + 239 等，`--all-features` 时 radicom lib 497、network lib 248、json 34、dict 4。

**总体结论**：

- 序列化/介质层的**骨架**（两遍 offset 重建、Bump 分配池、解析观察者、JSON/XML 写出）设计意图清楚、代码可读性好。
- 但有 **4 个高危正确性缺陷**：DICOMDIR 带 icon 时 `first/last root` offset 直接写错（写出的文件不符合 PS3.3 F.2.2，第三方读会建错树）；`CodeItem` 的 tag 模型整体错误，产出**违反 VR 的非法数据集**（三个独立缺陷，见 §2.2）；JSON 声明"剥离 group length"但实际**完全没有剥离**；10 个 `ISO_IR` 字符集被映射到 Windows 代码页，俄文/阿拉伯文等直接乱码。前 3 个在本 scope 内，第 4 个虽在 `encoding/charset.rs`（不在本 scope 目录），但它是本次测试审查最重要的例证，故一并记录（§2.11）。
- **测试质量是本次最严重的问题**。799 个测试函数里，真正具备判别力的不到一半；关键路径上存在**成体系的自我实现/恒真/假绿测试**：
  - DICOMDIR 的 offset 测试用"扫描自己写出的字节"来验证自己写出的 offset，因此上面第 1 个高危缺陷能全绿通过；
  - SR 的 code-item 测试把错误的 tag 常量当作期望值写进断言，因此第 2 个高危缺陷能全绿通过，并且**修好实现反而会让测试变红**；
  - JSON 的 golden 与本 crate 的实现共享同一个生成函数（`xtask regen-goldens` 调用被测函数），第 3 个缺陷在 golden 里被固化成"正确值"。
  - 字符集测试把错误映射当成期望值断言（§2.11），而语料里根本没有 ISO-8859-3/4/5 文件。
  - `docs/development/03-testing.md` §1/§5/§7 声称存在 `fuzz/` 目录、DCMTK/fo-dicom nightly interop、cargo-fuzz 目标——**`fuzz/` 目录不存在**，interop 的"外部对端"测试在未设环境变量时静默 `return`（永远绿）。
- **一条贯穿全报告的结论**：本仓库里任何"encode → decode"形式的自往返测试，只能证明**内部自洽**，永远无法证明**符合标准**。上面 4 个高危缺陷、以及 Lead 用真实 fo-dicom 差分测出的 8 个语料文件解析失败 + 2 个字符集文件 panic，全部发生在这条盲区里。

| 级别 | 数量 | 说明 |
| --- | --- | --- |
| 高 | 4 | 写出的文件/JSON 不符合规范，或字符集解出乱码；与 fo-dicom 不通或含义错误 |
| 中 | 7 | 规范语义偏差、与 fo-dicom 行为不一致 |
| 低 | 3 | 死选项、注释与实现相反、相等语义过严 |

另有性能问题 6 项（§3）、测试质量问题：恒真/自我实现 27 处、假绿 13 处、覆盖缺口 25 项（§4）。
§2.0 记录一项**经差分测试确认"radicom 正确、与 fo-dicom 等价"**的怀疑点（`(0008,1140)` 整条删除），供后续评审不再重复怀疑。

---

## 1. 功能缺失（对比 fo-dicom，仅限本 scope）

状态取值：**已实现** / **部分实现** / **缺失** / **有意不实现**。"缺失"仅在 grep 确认 repo 内无对应符号后标注。

### 1.1 DICOMDIR（`media/mod.rs`）

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 读记录树 + offset 拼接 | `DicomDirectory.cs`、`DicomDirectoryReaderObserver.cs:44` | 部分实现 | `media/mod.rs:583` `stitch`、`media/mod.rs:673` `item_offsets` | 用"扫描字节找 `FFFE,E000`"代替观察者的 `(uint)Position-8` 键；容忍度靠 `roots.is_empty()` 兜底，而非 `ParseDirectoryRecordNotExact` 的最近 offset |
| 写两遍 offset | `DicomDirectory.cs:399` `CalculateOffsets`/`SetOffsets` | 已实现 | `media/mod.rs` `rebuild`、`write_layout` | 带 icon 时 first/last root offset 的正确性已随 §2.1 修复（`55ab7b6`） |
| `AddFile` 四层去重链 | `DicomDirectory.cs:474` `AddFile` / `CreateImageRecord` | 已实现 | `media/mod.rs:324` `add_file`、`:389` `find_or_create_child` | Patient→Study→Series→Instance，按 UID 复用层级记录 |
| `DicomDirectoryEntry`（四层记录打包返回） | `DicomDirectory.cs:474`（返回 `DicomDirectoryEntry`） | 已实现 | `media/mod.rs` `DicomDirectoryEntry`；`add_file_entry` | 返回本次新建/复用的四条记录索引 |
| `RootDirectoryRecordCollection` / 每级遍历集合 | `DicomDirectory.cs`（`RootDirectoryRecordCollection` 等） | 部分实现 | `media/mod.rs:283` `walk`、`:276` `children` | 只给出 `(index, depth)` 与索引，没有强类型的层集合/迭代器 |
| `DirectoryRecord` 作为对象（`NextDirectoryRecord` / `LowerLevelDirectoryRecord` / `Key`） | `DicomDirectory.cs:62` `DicomDirectoryRecord` | 部分实现 | `media/mod.rs:158` `DirectoryRecord{dataset,parent}` | 用 `parent: Option<usize>` + 索引重建树，没有 `next`/`lower` 指针字段，也没有 `Key` |
| `DicomDirectoryRecord::ToString()` | `DicomDirectory.cs` | 已实现 | `Display for DirectoryRecord`（`b41359f`） | 渲染记录 dataset 的 dump |
| `FileSetID` 空值校验 | `DicomDirectory.cs`（`FileSetID` setter 校验空串） | 已实现 | `media/mod.rs` `try_new` | `new("")` 现在 panic 并给出明确信息，`try_new("")` 返回错误（`5f3ca60`；`empty_file_set_id_is_rejected`） |
| `AutoValidate` | `DicomDirectory.cs:44` | 已实现 | `DicomDirectory::validate`、`model/dataset.rs` `set_validation`、`validation::ValidationScopeGuard` | 可直接校验 DICOMDIR 记录树（FileSetID、父/根索引、环、可达性），也可用 Dataset 级校验开关；`walk` 对环已做去重保护 |
| 图标生成（`IconImageSequence`） | `Media/DicomIconImageSequenceBuilder.cs:28` | 已实现 | `media/icon.rs` `build_icon`、`attach_icon` | fo-dicom 的 builder 同样只接受 8-bit MONOCHROME2 灰度；radicom 一致（彩色源不产生灰度 icon 属对齐行为），`MAX_ICON_SIZE=128` 一致 |
| 图标失败的静默容忍 | `DicomDirectory.cs`（`AddIconSequence` 内 `catch { }`） | 已实现 | `media/mod.rs:377-382` | 走 `generator.generate(...)?`，**错误会上抛**，比 fo-dicom 更严格；行为差异见 §2.8 |
| `IIconGenerator(frameIndex)` 显式帧号参数 | `Media/IIconGenerator.cs` | 已实现 | `media/icon.rs` `generate_frame`（`c9091b4`） | trait 提供显式帧号；`explicit_frame_index_selects_the_requested_frame` 覆盖 |

### 1.2 文件扫描器（`media/scanner.rs`）

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 目录递归 + glob | `Media/DicomFileScanner.cs:26` | 已实现 | `media/scanner.rs:88` `scan_directory`、`:218` `Glob` | `*`/`?`/`**` 均支持，深度可限 |
| 进度/发现事件 | `DicomFileScanner.cs`（`Progress`/`FileFound`/`Complete`） | 已实现 | `media/scanner.rs` `ScanEvent::{Progress,File,Complete}`（`dad8d74`） | 含扫描完成事件与 `ScanSummary` |
| `Stop()` 取消 | `DicomFileScanner.cs:97` | 已实现 | `media/scanner.rs` `ScanControl::stop`（`ecc51ac`） | 显式取消控制 |
| `ProgressFilesCount`（进度回调间隔） | `DicomFileScanner.cs:48` | 已实现 | `media/scanner.rs` `ScanEvent::Progress` | fo-dicom 的 `ProgressFilesCount` 是"每 N 个文件发一次进度"的间隔；radicom 每个匹配文件都发一次 `Progress{scanned,matched}`，粒度更细（等价于 N=1），无需预统计总数 |
| `CheckForValidHeader`（只读 DICM 头快速过滤） | `DicomFileScanner.cs:158` | 已实现 | `ScanOptions::check_for_valid_header`（`fbf1f9e`） | 打开前先校验 PS3.10 头 |
| 异步（`Task.Run` + `IProgress`） | `DicomFileScanner.cs:26` | 有意不实现 | `media/scanner.rs:7-11`（模块注释说明理由） | 核心 crate 不带 async 依赖，由调用方放到阻塞线程；理由已在文档写明，可接受 |
| `DicomDirectoryBuilder` | `DicomFileScanner.cs:188` | 已实现 | `media/scanner.rs:281` | — |

### 1.3 结构化报告（`sr/`）

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 内容树（15 ValueType / 7 Relationship / Continuity） | `StructuredReport/DicomContentItem.cs` | 已实现 | `sr/content_item.rs:38` `ValueType`、`:186` `Relationship`、`:150` `ContentItem` | 数量与 fo-dicom 一致 |
| `StructuredReport` 打开/保存 | `StructuredReport/DicomStructuredReport.cs:33` `Open`/`:70` `Save` | 已实现 | `sr/structured_report.rs` `open`/`open_reader`/`save`/`write_to` | 支持路径与 `Read+Seek`/`Write` 流重载（fo-dicom 的 Stream 形态）；`SaveAsync` 不适用（核心 crate 无 async） |
| `StructuredReport` 继承 `DicomContentItem`（自身即根节点） | `DicomStructuredReport.cs:15`（`: DicomContentItem`） | 有意不实现 | `sr/structured_report.rs:16`（包一层 `DicomFile`） | radicom 用 `root_items()` 暴露根，组合优于继承，合理 |
| 单根 CONTAINER 校验 | `DicomStructuredReport.cs`（`AutoValidate`） | 已实现 | `sr/structured_report.rs:81` `validate` | 显式调用，非自动 |
| `CodeItem` 三段自动分派（short/long/URN） | `StructuredReport/DicomCodeItem.cs:24`（`CodeValue`/`LongCodeValue`/`URNCodeValue` 三选一） | **实现错误** | `sr/code_item.rs:23` `SHORT`、`:31` `LONG`、`:114` `needs_long` | 见 §2.2：模块的"短/长 tag 族"前提本身不成立；`SHORT` 整族非法，`LONG` 的 version tag 错，`LongCodeValue`/`URNCodeValue` 从未实现 |
| `DicomCodeItem::FromUrn` | `DicomCodeItem.cs:50` | 已实现 | `sr/code_item.rs` `from_urn`（`2ec3a5d`） | URN → CodeItem 入口 |
| `DicomCodeItem` 相等语义（Value+Scheme+Version） | `DicomCodeItem.cs:110` `Equals` | 已实现 | `sr/code_item.rs:39` `#[derive(PartialEq, Eq)]` | 字段含 `value`/`scheme`/`meaning`/`scheme_version`；`meaning` 也参与相等，比 fo-dicom 更严（fo-dicom 不比 meaning） |
| `DicomMeasuredValue`（decimal + units） | `StructuredReport/DicomMeasuredValue.cs` | 已实现 | `sr/content_item.rs` `MeasuredValue` | 数值按 DS 规则格式化并在超长/非有限时报错，支持单位 code sequence（§2.6） |
| `DicomReferencedSOP` | `StructuredReport/DicomReferencedSOP.cs` | 已实现 | `sr/content_item.rs:236` `ReferencedSop` | — |

### 1.4 匿名化（`anonymizer/`）

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| PS3.15 全表（生成、内嵌） | `DicomAnonymizerGenerated.cs` | 已实现 | `anonymizer/profile.rs:227` `basic`、`ps3.15.csv`（655 行） | 行数与 fo-dicom 一致 |
| 11 选项 × 6 动作矩阵 | `DicomAnonymizer.cs:29` `SecurityProfile` | **实现错误** | `anonymizer/profile.rs:259` `action_for` | "首行匹配即止"vs fo-dicom"每选项一张表"，见 §2.4 |
| 动作 D/Z/X/K/C/U | `DicomAnonymizer.cs:38` `SecurityProfileActions` | 已实现 | `anonymizer/profile.rs:60` `Action` | — |
| 正则 tag 匹配（私有组/曲线/覆盖层通配） | `DicomAnonymizer.cs:150`（`Dictionary<Regex,...>`） | 有意不实现（自写子集） | `anonymizer/profile.rs` 自写极小正则子集 | 只支持字面量、`[类]`、`{n}`；这是**有意**的轻依赖选择，PS3.15 全表用该子集即可精确匹配。`regex` 仅由 `rules/` 的独立 `regex-rules` feature 使用（§3.6 已解耦） |
| SQ 递归 | `DicomAnonymizer.cs:250` | 已实现 | `anonymizer/mod.rs:98` `walk`、`:111-114` | X 删整条、K 保留、其余递归 |
| UID 一致替换 | `DicomAnonymizer.cs:74` `ReplacedUIDs` | 已实现 | `anonymizer/mod.rs` `replace_uid`、`anonymize_in_place`、`replaced_uids` | 替换表在 `Anonymizer` 实例上（`Arc<Mutex<..>>`），跨调用/跨文件一致（§2.5 已修复） |
| 类型感知 blanking | `DicomAnonymizer.cs:290` | 已实现 | `anonymizer/mod.rs:195` `blank` | 按 VR 写空串/空字节 |
| PatientName/ID 替换值 | `DicomAnonymizer.cs:60` | 已实现 | `anonymizer/mod.rs:57` `with_patient` | — |
| 文件级 meta 同步 | `DicomAnonymizer.cs:170` | 已实现 | `anonymizer/mod.rs:70` `anonymize_file_in_place` | — |
| 自定义 profile / 注册 | `DicomAnonymizer.cs:29`（可 `Add` 行） | 已实现 | `anonymizer/profile.rs` `with_row`、`with_profile` | 可整体替换 CSV，也可用 `with_row` 追加单行（保持行序优先级） |
| 公开 `ReplacedUIDs` 访问器 | `DicomAnonymizer.cs:74` | 已实现 | `anonymizer/mod.rs` `replaced_uids()` | 每个 anonymizer 一份替换表 |

### 1.5 匹配/变换 DSL（`rules/`）

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 匹配规则 | `DicomMatchRules.cs`（11 个 `*DicomMatchRule` 类） | 已实现 | `rules/match_rule.rs:20` `MatchRule`（12 变体 = 11 + `Set`） | 与 fo-dicom 一一对应 |
| 变换规则 | `DicomTransformRules.cs`（15 个类） | 已实现 | `rules/transform.rs:26` `TransformRule`（15 变体） | 数量一致 |
| Modified Attributes Sequence 记录 | `DicomTransformRules.cs`（`AddOrUpdate` 前 `TryGetValues`） | 已实现 | `rules/transform.rs:67` `apply_recording`、`:267` `record` | — |
| `MatchRule::Regex` / `TransformRule::Regex` 可用性 | `DicomMatchRules.cs`（始终可用） | 已实现（opt-in） | `rules/match_rule.rs`、`transform.rs`（`#[cfg(feature="regex-rules")]`） | 独立 `regex-rules` feature 提供，避免匿名化被迫引入 `regex`（见 §3.6）；默认 feature 下仍不编译该变体 |

### 1.6 JSON / XML / 字典 / xtask

| 能力 | fo-dicom 参照 | 状态 | radicom 符号（file:line） | 说明 |
| --- | --- | --- | --- | --- |
| 单 dataset → JSON | `Serialization/DicomJson.cs:24` | 已实现 | `radicom-json/src/lib.rs:99` `dataset_to_json` | — |
| JSON → 单 dataset | `DicomJson.cs:48` | 已实现 | `lib.rs:~360` `json_to_dataset` | — |
| dataset 集合 → JSON 数组 | `DicomJson.cs:31` `ConvertDicomToJson(IEnumerable)` | 已实现 | `radicom-json/src/lib.rs` `datasets_to_json`（`ccb9468`） | QIDO-RS 多结果数组 |
| JSON 数组 → dataset 集合 | `DicomJson.cs:56` `ConvertJsonToDicomArray` | 已实现 | `radicom-json/src/lib.rs` `json_to_datasets`（`ccb9468`） | 同上 |
| group length 写出时剥离 | `JsonDicomConverter.cs` | 已实现 | `radicom-json/src/lib.rs` `dataset_to_value` | `(GGGG,0000)` 在每层都被跳过；`group_lengths_are_stripped_at_every_level` 覆盖 |
| 空 SQ 不写 `Value` | `JsonDicomConverter.cs` | **不一致** | `lib.rs:135-144`、`:172-176` | radicom 写 `"Value":[]`，fo-dicom 不写 |
| 空值组件写 `null` | `JsonDicomConverter.cs` | **不一致** | `lib.rs:214`、`:503-509` | radicom 写 `""`，fo-dicom 写 `null`（见 §2.7） |
| 数字默认模式 | `JsonDicomConverter.cs:69`（默认 `AsNumber`） | 有意不同 | `lib.rs:54-65`（默认 `PreferablyAsNumber`） | 差异见 §2.7，需明确声明 |
| XML 单向导出 | `Serialization/DicomXML.cs:22` | 已实现 | `radicom-json/src/xml.rs:27` `dataset_to_xml` | fo-dicom 同样只有写 |
| `privateCreator` 属性 | `DicomXML.cs:157` | 已实现 | `xml.rs:81-83` | — |
| 片段序列 XML 表示 | `DicomXML.cs:108`（按 VR 走 `InlineBinary`） | 已实现（与自身注释矛盾） | `xml.rs:43`、`:220` `write_fragments` | 注释 `:42` 说"不可表示"，实际会写 `<Fragment>`；见 §2.9 |
| 字典表 | `DicomDictionary.cs` | 已实现 | `crates/radicom-dict/src/lib.rs:8` | 5347 公有 / 4678 私有 / 235 creator / 1928 UID |
| 代码生成 | `Tools/` + 生成器 | 已实现 | `xtask/src/main.rs:120` `gen_dict`、`:435` `regen_goldens` | — |
| `DicomDatasetDumper` 的行格式 | `Log/DicomDatasetDumper.cs:95`（`tag VR [value] #len, keyword`） | 有意不同 | `dump/mod.rs:54`（`tag VR keyword value`） | 无 `#长度`、无 `Item:`、无 `_pad + "> "` 层级前缀；`docs/01:202` 的 "mirroring FO-DICOM.Dump" 因此只是"功能对齐"，行格式并不对齐 |

---

## 2. 正确性缺陷（本 scope）

### 2.0 【已验证等价，非缺陷】匿名化对 `(0008,1140)` 整条删除是正确的

立项怀疑：对含 `ReferencedImageSequence (0008,1140)` 的数据集匿名化后整条序列消失，而非递归进入子项，疑似"误删嵌套的 `ReferencedSOPInstanceUID`"。

**核查结论：radicom 正确。** PS3.15 Table E.1-1 在 basic profile 下把 `(0008,1140)` 标为 **X（remove）**，`ps3.15.csv` 里的 `0008,1140;X;...` 与之相符，因此整条序列（含其所有子项）被删除是规范要求的行为。Lead 用真实 fo-dicom 6.0 构建对同一输入做了对照：fo-dicom 同样报 `ReferencedImageSequence present after anonymize: False`。两侧行为一致。

同时已验证（Lead 的差分测试）：14 个 PHI tag、0 泄漏，逐 tag 结果与 fo-dicom 完全一致——`PatientName`/`PatientID`/`BirthDate`/`Sex`/`AccessionNumber`/`ReferringPhysician` → 空值；`OtherPatientIDs`/`OtherPatientNames`/`Telephone`/`InstitutionName`/`StationName`/`StudyDescription`/`RequestingPhysician`/`PerformedStationAETitle` → 删除。

`ps3.15.csv` 与 fo-dicom 内嵌的 `DefaultProfile` **逐字节相同**（655 行，diff 干净），所以本报告不对该表的**内容**做审查——它无需审查。本报告 §2.4 指出的是**查表算法**（`action_for` 的重叠行语义）而非表数据，两者是独立的：表本身正确，但读表方式让某些列取不到值。

### 2.1 【高】带 IconImageSequence 时 DICOMDIR 的 first/last root offset 写错

`crates/radicom/src/media/mod.rs:669-686`（`item_offsets`），调用点 `:438-457`（`rebuild`）。

```rust
// media/mod.rs:673
fn item_offsets(bytes: &[u8], count: usize) -> Option<Vec<u32>> {
    // ... 扫描全部 FFFE,E000 ...
    Some(positions[positions.len() - count..].to_vec())   // ← 取"尾部 count 个"
}
```

`item_offsets` 假设"record sequence 是文件里最后一个序列，所以记录 item 头是尾部连续的一段"。这个假设在**记录内部再嵌序列时失效**：`IconImageSequence (0088,0200)` 会给 IMAGE 记录多加一个 `FFFE,E000` 头，于是 `positions.len() > count`，尾部切片取到的是**错位的那一段**。

实测（clean repo，1 条 PATIENT/STUDY/SERIES/IMAGE；我与 Lead 各自独立构建，绝对偏移略有不同因 FileSetID/SOPInstanceUID 长度不同，但错位量与结论完全一致）：

| 场景 | 记录数 | 文件中全部 item 头位置 | 真实 PATIENT 位置 | 写出的 `(0004,1200)` | 结论 |
| --- | --- | --- | --- | --- | --- |
| 无 icon | 4 | `[368, 436, 506, 578]` | 368 | **368** = `positions[0]` | ✓ PATIENT |
| 带 icon | 4 | `[368, 436, 506, 578, 722]` | 368 | **436** = `positions[1]` | ✗ STUDY |
| 无 icon（Lead 构建） | 4 | `[372, 440, 510, 592]`，792 B | 372 | 372 = `positions[0]` | ✓ PATIENT |
| 带 icon（Lead 构建） | 4 | `[372, 440, 510, 592, 736]`，1004 B | 372 | 440 = `positions[1]` | ✗ STUDY |

两次独立构建给出同一个错位规律：**`first_root` 恰好下移一个记录**（PATIENT → STUDY）。

**关键限定条件（决定了为什么测试全绿）**：该缺陷**只在 record sequence 里再嵌序列时触发**。没有 icon 时 `positions.len() == count`，尾部切片恰好等于全部记录，`positions[0]` 就是 PATIENT，所以 §4.2(b) 的三个 offset 测试（全部无 icon）都通过。一旦有任何嵌套（`IconImageSequence (0088,0200)`，或任何未来带私有序列的记录），`positions.len() > count`，尾部切片整体右移，根指针就落到 STUDY 上。

带 icon 时 `(0004,1200)`/`(0004,1202)` 都写成 436（Lead 构建里是 440），而那处的记录是 STUDY。

**后果**：写出的 DICOMDIR 不符合 PS3.3 F.2.2。符合规范的读者（fo-dicom 的 `DicomDirectoryReaderObserver` 以绝对位置为键）会从 STUDY 记录开始拼根链，得到一棵缺 PATIENT 层的错树。radicom 自己的读者看不出来——`stitch()`（`:583`）走的是同一条 `item_offsets`，切片方式一致，错误互相抵消；而且 `:653-658` 还有 `roots.is_empty()` 两级兜底，任何 offset 解析失败都会退化成"未到达且未被指向的记录都是根"，正好蒙对。

**期望行为**：记录 item 的位置应由写出过程直接给出（`write_dataset_into` 已知每个 item 头的偏移），或按 `DirectoryRecordSequence` 的 payload 区间限定扫描范围，而不是取全文件尾部一段。

### 2.2 【高】`CodeItem` 的 tag 模型整体错误——三个独立缺陷，且模块的核心设计前提不成立

`crates/radicom/src/sr/code_item.rs:22-36` 定义两个"tag 族"：

```rust
pub static SHORT: CodeTags = CodeTags {
    value: Tag::new(0x0018, 0x0090),           // DataCollectionDiameter (DS)
    scheme: Tag::new(0x0018, 0x0100),          // 字典中不存在
    meaning: Tag::new(0x0018, 0x0102),         // 字典中不存在
    scheme_version: Tag::new(0x0018, 0x0104),  // 字典中不存在
};
pub static LONG: CodeTags = CodeTags {
    value: Tag::new(0x0008, 0x0100),           // CodeValue (SH)     ✓
    scheme: Tag::new(0x0008, 0x0102),          // CodingSchemeDesignator (SH) ✓
    meaning: Tag::new(0x0008, 0x0104),         // CodeMeaning (LO)   ✓
    scheme_version: Tag::new(0x0008, 0x0106),  // ContextGroupVersion (DT) ✗
};
```

模块文档（`:1-7`）把"按值长度在短/长 tag 族间分派"当成 DICOM 的机制。**这个前提本身是错的**，所以缺陷是系统性的而非笔误。下面三个缺陷相互独立，需分别修。

#### 缺陷 a：SHORT 整族 tag 错误，写出的是**无效数据集**（不只是"无意义"）

在 `crates/radicom-dict/src/generated/dict.rs` 中逐条核对：

| tag | 字典实际内容 | radicom 当作 | 后果 |
| --- | --- | --- | --- |
| `(0018,0090)` | `DataCollectionDiameter`，**VR=DS** | CodeValue | 把字符串 `"T-A0100"` 写进 DS 元素 → **非数值内容落在数值 VR 里，数据集校验必然失败** |
| `(0018,0100)` | **字典中不存在**（keyword 回落 `Unknown`） | CodingSchemeDesignator | 写出时 VR 取 `Vr::Sh`，但与字典登记不符 |
| `(0018,0102)` | **字典中不存在**（`Unknown`） | CodeMeaning | 同上（实测被写成 LO） |
| `(0018,0104)` | **字典中不存在**（`Unknown`） | CodingSchemeVersion | 同上 |

实测 `CodeItem::new("T-A0100","SRT").with_meaning("Chest").with_version("2")` 的写出结果：

```
(0018,0090) written VR=SH  dict_keyword=DataCollectionDiameter  value="T-A0100"   ← DS 槽里塞字符串
(0018,0100) written VR=SH  dict_keyword=Unknown                 value="SRT"
(0018,0102) written VR=LO  dict_keyword=Unknown                 value="Chest"
(0018,0104) written VR=SH  dict_keyword=Unknown                 value="2"
```

**比"tag 不存在"更严重**：`(0018,0090)` 是**已注册的 DS 元素**，往里写 `"T-A0100"` 产生的是**违反 VR 定义的非法数据集**。若下游调用 `dataset.validate()`，这会直接报错；若写出到文件，第三方读到的 `DataCollectionDiameter` 是个无法解析的 DS。

#### 缺陷 b：LONG 的 `scheme_version` 用错 tag 且 VR 冲突

`(0008,0106)` 是 `ContextGroupVersion`，**VR=DT**（日期时间）；`CodingSchemeVersion` 是 `(0008,0103)`，**VR=SH**。实测带 version 的长 code 把 `"3"` 写进 `(0008,0106)` 且 VR 标成 SH：

```
(0008,0106) written VR=SH  dict_keyword=ContextGroupVersion  value="3"
```

即一个 DT 元素被当成 SH 写入，值是 `"3"`——既不是合法 DT，也不是合法 SH 语义下的 version。

#### 缺陷 c：`LongCodeValue` 与 `URNCodeValue` 完全未实现

`needs_long()`（`:114`）宣称按长度/前缀分派，但两个"分支"落到的是同一个 `(0008,0100)`：

| 输入 | 实际写出 | 应为 |
| --- | --- | --- |
| 17 字符值 `"12345678901234567"` | `(0008,0100)` VR=SH，值 17 字符 | `(0008,0119) LongCodeValue` **VR=UC** |
| `"urn:oid:1.2.3"` | `(0008,0100)` VR=SH | `(0008,0120) URNCodeValue` **VR=UR** |

`(0008,0100)` 的 VR 是 **SH，上限 16 字符**，所以 17 字符值写进去**长度越界**，是非法元素。UR 同理。字典里 `(0008,0119)` 与 `(0008,0120)` **存在且 VR 正确**（UC / UR），代码从未引用它们（grep `0x0119` / `0x0120` 在 `sr/` 下无命中）。

#### 规范事实与正确模型

DICOM 的 coded entry **只有一族 tag**，value 槽位按内容在三个 tag 中选一个：

| 角色 | tag | VR | 约束 |
| --- | --- | --- | --- |
| code value（≤16 字节） | `(0008,0100)` CodeValue | SH | ≤16 |
| code value（>16 字节） | `(0008,0119)` LongCodeValue | UC | 无 16 限制 |
| code value（URN） | `(0008,0120)` URNCodeValue | UR | `urn:` / `http:` 等 |
| coding scheme designator | `(0008,0102)` | SH | — |
| coding scheme version | `(0008,0103)` | SH | — |
| code meaning | `(0008,0104)` | LO | — |

fo-dicom `StructuredReport/DicomCodeItem.cs` 正是这个模型：三个 value tag 常量（`:31`/`:39`/`:44`）按值择一，`Scheme`/`Version`/`Meaning` 恒为 `(0008,0102)`/`(0008,0103)`/`(0008,0104)`。**没有"短 tag 族 vs 长 tag 族"这回事。**

对照真实 SR 文件 `tests-data/sr/test_SR.dcm` 的 `ConceptNameCodeSequence` 首条 item：`(0008,0100)`, `(0008,0102)`, `(0008,0104)` —— 与该模型一致，与 radicom 的 SHORT 族完全不同。

**期望行为**：删除 `SHORT`/`LONG` 两个 `CodeTags` 静态量，改为单一 `CodeTags { value: 按内容解析, scheme: (0008,0102), scheme_version: (0008,0103), meaning: (0008,0104) }`，且 `write_into` 为每个 tag 使用字典 VR（SH/UC/UR/SH/LO）而非一律 `Vr::Sh`。

**文档连带错误**：`docs/design/06-serialization-media.md:91` 与 `docs/development/02-feature-list.md:174`（SR-02）都把"`(0018,0090..)` 短格式"写成已实现特性——文档忠实复述了代码里的错误前提，因此**不能作为实现正确性的证据**。

**测试连带错误**：`code_item.rs:130-155` 的三个测试直接断言 `SHORT.value` / `LONG.value` 的内容，等于把上述 a/b/c 全部钉成期望值。修好实现后这三个测试会变红（详见 §4.2(a)）。

### 2.3 【高】DICOM JSON 声明"剥离 group length"，实际完全没剥离

`crates/radicom-json/src/lib.rs:109-123`（`dataset_to_value`）对 `dataset.items()` 无条件写入每个键，没有 `element() == 0x0000` 的过滤。

实测 `tests-data/explicit-le/GH178.dcm`：

```
radicom: 65 个顶层键，其中 group-length 键 = ["00080000","00100000","00180000",
                                                 "00200000","00280000","00400000"]
嵌套： (0008,1110) item 0 内还有 "00080000"
fo-dicom: 顶层 0 个 group-length 键
```

递归同样漏：`sequence_to_value`（`:135-144`）对每个 item 调 `dataset_to_value`，所以嵌套 item 里的 `(gggg,0000)` 一起被写出。

**三重矛盾**：

- `docs/development/02-feature-list.md:167`（JSON-01）明确承诺 "group length stripped"；
- `docs/design/06-serialization-media.md:24` 写 "Group-length `(GGGG,0000)` elements are skipped on write"；
- `crates/radicom/tests/corpus.rs:88-89` 的语义往返测试**必须**调用 `strip_group_lengths` 才能过——因为 writer 会丢 group length 而 JSON 不丢，两边口径不一致。

**期望行为**：`dataset_to_value` 里加一条 `if element.tag().element() == 0x0000 { continue; }`（递归天然生效）。修好后 golden 必须重新生成，且 `corpus.rs` 的 `strip_group_lengths` 注释也要改。

### 2.4 【中】匿名化 profile 的"首行匹配"语义与 fo-dicom 不一致，导致 CleanGraph 等选项失效

`crates/radicom/src/anonymizer/profile.rs:258-264`：

```rust
pub fn action_for(&self, tag: &str, option: AnonOption) -> Option<Action> {
    self.rows.iter()
        .find(|row| row.pattern.matches(tag))       // ← 先按 tag 找第一行
        .and_then(|row| row.action_for(option))     // ← 再看这一行有没有该选项的格子
}
```

fo-dicom `DicomAnonymizer.cs:29` 的 `SecurityProfile` 是 `Dictionary<Regex, SecurityProfileActions>`，**按选中的选项分别构建**：某行在该选项下格子为空就**根本不插入**，因此后面更具体的行可以匹配到。

`ps3.15.csv` 第 1 行是 `[0-9A-F]{3}[13579BDF],[0-9A-F]{4};X;C;;;;;;;;;`（奇数私有组 → X；只有 RetainSafePrivate 列有值）。它匹配**所有**奇数组私有 tag，于是把后面那些行的 CleanGraph 列全部遮住。

实测（12 个 tag × 11 个选项，与 fo-dicom 语义逐格比对，差异 3 处）：

| 选项 | tag | radicom | fo-dicom |
| --- | --- | --- | --- |
| `CleanGraph` | `5011,1234` | `None` | `Some(Clean)` |
| `CleanGraph` | `6011,3000` | `None` | `Some(Clean)` |
| `CleanGraph` | `50FF,0001` | `None` | `Some(Clean)` |

**后果**：`CleanGraph`（清理曲线/覆盖层图形）对私有曲线/覆盖层 tag 完全不起作用——图表数据不被清洗就外发，属于去标识化safety问题。

现有 7 个 profile 测试（`profile.rs:275-330`）**没有一个覆盖"同一 tag 在多个选项下的不同动作"**，所以这个偏差全绿通过。

**期望行为**：`action_for` 按选项过滤后再匹配行（即"跳过该选项格子为空的行"），或按选项预构建 11 张表。

### 2.5 【中】UID 替换 map 是"每次调用"而非"每个 anonymizer"，跨调用/跨文件不一致

`crates/radicom/src/anonymizer/mod.rs:64-67`：

```rust
pub fn anonymize_in_place(&self, dataset: &mut Dataset) -> Result<(), Error> {
    let mut replaced = HashMap::new();   // ← 每次调用新建
    self.walk(dataset, &mut replaced)
}
```

fo-dicom 的 `ReplacedUIDs`（`DicomAnonymizer.cs:74`）是实例成员，注释明确说"用于在一个文件集（多次调用）内保持一致"。

实测：同一个 `Anonymizer`，两次对同一个 `StudyInstanceUID="1.2.3.4.5"` 的数据集匿名化，得到两个不同的 UID：

```
2.25.304105865551860065616387517730286930432
2.25.288026475585633819283971922925083593916
```

**后果**：批量匿名化一个 study 的多个文件后，原本共享 StudyInstanceUID 的实例会各自拿到不同 UID，study 被打散；而 PS3.15 的 U 动作（以及 ANON-01 自述的 "consistent UID replacement"）要求同一原始 UID 恒映射到同一新 UID。

**期望行为**：把 map 提到 `Anonymizer` 实例上（`&self` + `Mutex<HashMap>`，或 `&mut self`），并提供公开访问器对应 fo-dicom 的 `ReplacedUIDs`。

### 2.6 【中】`MeasuredValue` 的数值精度与单位

`crates/radicom/src/sr/content_item.rs:200`：`MeasuredValue` 的 `NumericValue (0040,A30A)` 以字符串保存。写入时若调用方给的是 `f64`，往返可能出现尾数变化（DS ≤16 字符，`0.1+0.2` 之类会写成 `0.30000000000000004` 超长）。fo-dicom 用 `decimal`（`DicomMeasuredValue.cs:20`），天然无此问题。**建议**：写入前按 DS 规则格式化/截断并在超长时报错，而不是静默写非法长度。

### 2.7 【中】JSON 三处与 fo-dicom 的默认行为分歧（需显式声明并加测试，而非默认放行）

| # | 现象 | radicom 位置 | radicom 输出 | fo-dicom 输出 | 规范依据 |
| --- | --- | --- | --- | --- | --- |
| a | 空 SQ 带 `"Value":[]` | `lib.rs:135-144`、`:172-176` | `{"vr":"SQ","Value":[]}` | `{"vr":"SQ"}` | PS3.18 Annex F：无 item 时不写 `Value` |
| b | 空值组件写 `""` 而非 `null` | `lib.rs:214`、`:503-509` | `{"Value":["","ISO 2022 IR 58"]}` | `{"Value":[null,"ISO 2022 IR 58"]}` | PS3.18 Annex F：空值组件为 `null` |
| c | IS 默认保留字符串 | `lib.rs:54-65`（`PreferablyAsNumber`） | `{"Value":["0001"]}` | `{"Value":[1]}` | fo-dicom 默认 `AsNumber`（`JsonDicomConverter.cs:69`） |

c 项尤其要注意：radicom 的 `PreferablyAsNumber` 拒绝 `"0001"` 是因为 `format_number_like(1.0) != "0001"`（`:304-313`），这是"无损"哲学——**本身可辩护，但它是与 fo-dicom 默认行为的静默分歧**，且 `tests/goldens.rs` 与 `lib.rs` 的测试都用默认选项，等于把这个分歧固化进 golden。报告建议：保留 `PreferablyAsNumber` 作为默认（更安全），但（1）在 `docs` 里明确写成"与 fo-dicom 默认不同"，（2）为 `AsNumber` 与 `PreferablyAsNumber` 各加一条针对 `"0001"` 的显式断言。

b 项还有一个不对称：`value_to_text` 把 `Value::Null` 读回成 `String::new()`（`:513`），于是读 `[null,"ISO 2022 IR 58"]` 会得到 `"\\ISO 2022 IR 58"`——**往返不保持**。同时 a 项有个更严重的读取缺陷：fo-dicom 形状 `{"vr":"SQ"}` 被 `attribute_to_item:419` 读成 `DicomElement::new_empty(tag, Vr::Sq)`，即**一个 VR 为 SQ 的普通 element，而不是空序列**；radicom 自己的形状 `{"vr":"SQ","Value":[]}` 才读成 `DicomSequence`。两者都会再写出 `{"vr":"SQ","Value":[]}`，所以自测往返一致，但与 fo-dicom 的 JSON 互读会改变内部类型。

### 2.8 【中】图标生成失败会中断整个 DICOMDIR 构建

`crates/radicom/src/media/mod.rs:377-382` 用 `generator.generate(&mut source)?`，任何错误都通过 `?` 上抛。fo-dicom `DicomDirectory.cs:596` 的 `AddIconSequence` 用裸 `catch { }` 吞掉全部异常，注释写明"图标失败不应阻止 DICOMDIR 创建"。

**后果**：批量构建时，一个形态少见的实例（例如带 palette 的 8-bit 彩色）会让 `add_file_with_icon` 直接失败，而调用方通常只想要"没有 icon"。这是可用性缺陷，且与 fo-dicom 语义相反。**建议**：`add_file_with_icon` 内部把 `generate` 的 `Err` 降级为"无 icon"，与 fo-dicom 对齐（或加一个 `IconErrorPolicy`）。

### 2.9 【低】XML 注释与实现相反

`crates/radicom-json/src/xml.rs:42` 注释 "fragment sequences are not representable in the XML model"，但 `:43` 立刻调用 `write_fragments`（`:220`）把它们写成 `<Fragment>` 元素。同时 `:14-17` 的模块文档也只说"sequences nest their items"。实际行为（写片段）与 fo-dicom 一致，注释才是错的。无功能影响，但会误导后续维护者。

### 2.10 【已修复】`JsonOptions::validate_on_read` 现已接入读取路径

`crates/radicom-json/src/lib.rs:72` 声明 `pub validate_on_read: bool`，`:81` 在 `Default` 里设为 `false`。但整个 crate 里没有任何地方读它——读取入口 `pub fn json_to_dataset(json: &str, _options: &JsonOptions)`（`lib.rs:356`）把 options 参数命名为 `_options` 并**整体忽略**（`value_to_dataset` 也不收 options）。

fo-dicom 的 `DicomJsonConverter` 有 `autoValidate`（`JsonDicomConverter.cs:69`，默认 `true`），读取后会跑数据集校验。radicom 声明了这个开关但没有接上，属于"文档/API 承诺与实现不符"。要么接上（读后调 `dataset.validate()`），要么删掉该字段。

### 2.11 【高】ISO-8859 系字符集被映射到 Windows 代码页，俄文/阿拉伯文等产生乱码

虽然 `encoding/charset.rs` 不在本 scope 的四个目录里，但它是 charset 语料与 `corpus.rs` 的宿主，且 Lead 的差分测试确认了故障现象；在此记录，因为**测试审查角度**上它是本报告 §4 最重要的例证之一。

`crates/radicom/src/encoding/charset.rs:19-29` 的映射表：

| 名称 | 规范要求（PS3.3 C.12.1.1.2） | radicom 实际使用 | fo-dicom `DicomEncoding.cs` |
| --- | --- | --- | --- |
| `ISO_IR 100` | ISO-8859-1 (Latin-1) | `windows-1252` | `iso-8859-1`（:143） |
| `ISO_IR 101` | ISO-8859-2 (Latin-2) | `windows-1250` | `iso-8859-2` |
| `ISO_IR 109` | ISO-8859-3 (Latin-3) | **`windows-1253`**（希腊语） | `iso-8859-3`（:151） |
| `ISO_IR 110` | ISO-8859-4 (Latin-4) | **`windows-1251`**（西里尔） | `iso-8859-4`（:152） |
| `ISO_IR 126` | ISO-8859-7 (Greek) | `windows-1253` | `iso-8859-7` |
| `ISO_IR 127` | ISO-8859-6 (Arabic) | `windows-1256` | `iso-8859-6` |
| `ISO_IR 138` | ISO-8859-8 (Hebrew) | `windows-1255` | `iso-8859-8` |
| `ISO_IR 144` | ISO-8859-5 (Cyrillic) | **`windows-1251`** | `iso-8859-5`（:156） |
| `ISO_IR 148` | ISO-8859-9 (Latin-5/Turkish) | `windows-1254` | `iso-8859-9` |
| `ISO_IR 166` | TIS-620 (Thai) | `windows-874` | `iso-8859-11` |

**实测解码错值**（clean repo，直接调 `charset_encoding`）：

| 输入 | radicom 输出 | 正确输出 |
| --- | --- | --- |
| `ISO_IR 109` + 字节 `E8 E9` | `"θι"`（希腊字母） | `"èù"`（ISO-8859-3 的 è/ù） |
| `ISO_IR 110` + 字节 `C8 D8` | `"ИШ"`（西里尔） | `"ČØ"`（ISO-8859-4） |
| `ISO_IR 144` + 字节 `B0 B1` | `"°±"` | `"АБ"`（ISO-8859-5 的 А/Б） |

同一张表在 `:37-48` 对 `ISO 2022 IR *` 犯了同样的错（含 `:40` 的 `ISO 2022 IR 110` → `windows-1251`）。

**为什么 `IS0_IR 100`/`101` 在实践中"看起来没事"**：windows-1252/1250 在 ASCII 与大部分常用字符上与 ISO-8859-1/2 重合，只在 0x80–0x9F 与少数码位分歧。但 `109`/`110`/`144` 是**完全不同**的字符集被当成别的语言解码——`ISO_IR 144`（俄文）用 windows-1251 解会得到拉丁符号，`ISO_IR 110`（波罗的海）用 windows-1251 解会得到西里尔字母。

**测试为什么全绿**（这条是 §4 的核心教训）：

1. `tests-data/charset/` 只有 `chrGB2312` / `chrJapMulti` / `chrSQEncoding` 三个文件，**全部是 GB2312/ISO-2022-JP**，没有一个覆盖 ISO-8859-3/4/5/6/7/8。Lead 的差分测试里出问题的 `chrRuss` / `chrArab` **不在本 repo 语料内**。
2. `charset.rs` 的 13 个测试里，唯一涉及 `ISO_IR 110` 的断言是 `:437`：
   ```rust
   let koi = Encoding::for_label(b"windows-1251").unwrap();
   assert_eq!(charset_for_encoding(koi), Some("ISO_IR 110"));
   ```
   即"**反向**映射把 windows-1251 认作 ISO_IR 110"——它把错误映射当成期望值来断言，性质与 §4.2(a) 的 `code_item.rs` 测试完全相同：**把缺陷钉成正确值**。
3. 因此整条字符集链路"编码 → 解码"自洽往返全过，只有与真实 ISO-8859 数据对照才会暴露。

**修正方向**：把 10 个 `ISO_IR` 改成 `encoding_rs` 的 ISO-8859 系列（`encoding_rs` 提供 `ISO_8859_2`…`ISO_8859_16` 的 compat 形式；`ISO-8859-3/4/5/6/7/8` 在 `encoding_rs` 里以 `windows-*` 之外的 `iso-8859-*` label 可取），或改用 `encoding_rs::Encoding::for_label(b"iso-8859-5")`。同时补 `chrRuss` / `chrArab` 到语料。

### 2.12 【低】`CodeItem` 相等语义包含 `meaning`，比 fo-dicom 严格

`crates/radicom/src/sr/code_item.rs:39` 用 `derive(PartialEq)`，四个字段全参与。fo-dicom `DicomCodeItem.cs:110` 的 `Equals` 只比 `Value` + `Scheme` + `Version`，不比 `Meaning`（同一 code 的描述文本可以不同）。radicom 更严格会让"同 code 不同描述"被判定为不等。若是有意（更安全），应在文档声明。

---

## 3. 性能问题（本 scope）

均在 clean repo、`--release`、Windows 本机测得，数字是"每次调用"的均值。

### 3.1 `SecurityProfile::basic()` 每次调用解析 655 行 CSV（≈316 µs）

`anonymizer/profile.rs:227` 每次 `SecurityProfile::basic()` 都重新 `parse(include_str!(...))`：对 655 行做 `split`、逐行 `TagPattern::new`（含字符类编译）、`Action::parse`。`Anonymizer::basic()`（`anonymizer/mod.rs:42`）也调它，所以**每构造一个 anonymizer 就付出一次全表解析**。

实测：`SecurityProfile::basic()` = **315.7 µs**；`Anonymizer::basic()` = **309.1 µs**（几乎全在这）。相比之下真正做活的 `anonymize_in_place` 对 500 个私有元素只要 **212.8 µs**。

**最小修法**：包一层 `once_cell::sync::Lazy<Arc<SecurityProfile>>`（workspace 已有 `once_cell`），`basic()` 返回 clone/clone-on-write，或返回 `&'static`。

### 3.2 `action_for` 每元素线性扫全表（O(rows) × O(elements)）

`anonymizer/profile.rs:259` 对每个元素遍历最多 655 行并做 `TagPattern::matches`（内部又是字符级匹配）。500 元素 × 655 行 ≈ 32.8 万次模式匹配。实测 500 元素 ≈ 213 µs 里绝大部分在此。

**最小修法**：按 tag 精确命中先查一个 `HashMap<u32, &ProfileRow>`（`ps3.15.csv` 里绝大多数行是字面 tag），只对通配行回退线性扫。

### 3.3 `dump_dataset` 每层重复取字典、每元素 clone tag

`dump/mod.rs:41` 在 `write_level` 内部调 `default_dictionary()`（虽返回 `Arc`，仍需锁），`:44` 对每个 item `item.tag().clone()`（`Tag` 含 `Option<Arc<PrivateCreator>>`，clone 会动引用计数），`:49` 再 `dictionary.entry(&tag)` 克隆 `Arc<DictionaryEntry>`。

实测 GH178（65 元素）：`dump_dataset` = **113.8 µs**，`to_dump_string` = **95.6 µs**。

**最小修法**：把 `default_dictionary()` 提到 `dump_dataset` 只取一次；tag 用 `item.tag()` 借用而不 clone。

### 3.4 `anonymize_in_place` 先整体克隆 dataset

`anonymizer/mod.rs:100`：`let items: Vec<DicomItem> = dataset.items().cloned().collect();` —— 为了在遍历中安全修改，把**每个 item 都克隆一遍**。`DicomItem::Element` 克隆包含 `SharedBuffer`（`Arc`，浅拷贝）+ `ElementValue`；`DicomItem::Sequence` 克隆是**深拷贝整棵子树**。

实测：1 MB pixel data → 139 µs；8 MB → 273 µs；32 MB → 238 µs（pixel data 是 `Arc` 缓冲，未真正复制，所以不随体量线性增长——但序列型 SR/DICOMDIR 会）。

**最小修法**：先收集 `Vec<Tag>`（只收集要删的），再用 `dataset.item(&tag)` 借读判动作；确实需要改时才取可变引用。或者用两层循环"先算动作列表，再逐条应用"。

### 3.5 `rebuild()` 反复整文件序列化直到 offsets 收敛

`media/mod.rs:447-456`：`loop { write_layout; to_bytes; item_offsets; if measured == positions return }`。每次迭代是**一次完整文件序列化**，最少 2 次（初始 `positions` 全 0 → 测出真值 → 再写一次），每次 `item_offsets` 又扫全文件每个字节。

实测 `to_bytes`（release）：5 记录 100.7 µs / 25 记录 244.7 µs / 100 记录 **1.24 ms**；`add_file` × 800 = 1.78 ms 但 `to_bytes` = **9.57 ms**——即构建成本几乎全在收尾的 rebuild。**100 条记录 1.24 ms、1000 条约 12 ms**，且 `item_offsets` 是 O(bytes) × O(迭代次数)，对大 DICOMDIR 呈二次增长。

**最小修法**：`write_dataset_into` 已经知道每个 item 头的绝对偏移；让 writer 回传 item 偏移表（或用 `trace`/观察者钩子），一次写完即可，不必"写—读回—再写"。次选：`item_offsets` 只扫描 `(0004,1220)` 的 payload 区间，而非 `0..bytes.len()`。

### 3.6 【已修复】`anonymize` feature 拉入 `regex`，但匿名化本身不用 `regex`

现已拆成独立 feature：`regex-rules = ["dep:regex"]`，`anonymize = []`；`rules/` 中 `Regex` 变体改由
`#[cfg(feature = "regex-rules")]` 控制。开匿名化不再引入 `regex`，需要正则规则时单独启用
`regex-rules`（两者可独立组合，已验证 `--features anonymize` 与 `--features regex-rules` 各自可编译）。

---

## 4. 测试审查（主交付物）

### 4.1 总体结论

**测试函数计数**（用 `^\s*#\[(tokio::)?test\]` 全量扫描 + 实际运行核对）：

| crate / suite | 测试函数数 | 备注 |
| --- | --- | --- |
| `radicom` lib（内联 `#[cfg(test)]`） | **497** | 默认 feature 下跑 **372** 个；`render` 关闭时 125 个不编译 |
| `radicom` `tests/cli_dump.rs` | 1 | — |
| `radicom` `tests/corpus.rs` | 5 | INF-06 语料 |
| `radicom` `tests/fuzz.rs` | 3 | 纯 std 字节扫描 |
| `radicom` `tests/goldens.rs` | 1 | dump golden |
| `radicom-dict` lib | 4 | 表格形状 |
| `radicom-json` lib | 34 | 23 个 `lib.rs` + 11 个 `xml.rs` |
| `radicom-json` `tests/goldens.rs` | 1 | JSON golden |
| `radicom-network` lib | 248 | 默认 feature 下跑 **239** 个 |
| `radicom-network` `tests/fuzz.rs` | 1 | — |
| `radicom-network` `tests/goldens.rs` | 1 | PDU golden |
| `radicom-network` `tests/interop.rs` | 3 | 2 个真 TCP 环回 + 1 个外部对端（会静默跳过） |
| `xtask` | **0** | 507 行代码，零测试 |
| **合计** | **799**（扫描）/ 全部可运行（`--all-features`） | — |

**测试"规模"与"判别力"严重不匹配**。逐文件读完后，把 799 个测试函数分四类：

| 类别 | 估计占比 | 说明 |
| --- | --- | --- |
| 有判别力 | ~45% | 断言了具体值/具体错误变体，改坏实现会红 |
| 只验证"不 panic" | ~15% | 8 个完全无断言 + 一批只有 `is_ok()`/`is_some()` |
| 自我实现 / 恒真 | ~25% | 断言自己刚构造的值，或断言常量等于自己的字面量 |
| 与被测实现共享生成逻辑 | ~15% | golden 由被测函数自己生成，只能发现漂移 |

**最重要的结论**：本 scope 的 3 个高危缺陷（§2.1 DICOMDIR offset、§2.2 CodeItem tag、§2.3 JSON group length）**全部**有"看起来在测、实际测不到"的测试在守护它们。其中 §2.2 更糟：`a_short_code_writes_the_short_tag_family`（`code_item.rs:130`）把错误的 tag 常量写进断言，**修好实现会让这个测试变红**。

**Lead 用真实 fo-dicom 构建做出的差分测试，给出了这套测试体系盲区的定量证据**——以下全部是"radicom 全绿但实际有问题"的情况：

| 现象 | radicom 测试状态 | 为什么测不到 |
| --- | --- | --- |
| 8 个 fo-dicom 语料文件**解析失败**（`GH133`、`GH626`、`GH179B`、`GH177…`、`GH1986`、`CR-ModalitySequenceLUT`、`OutOfBoundsOverlay`、`D_CLUNIE_CT1_RLE_FRAGS`） | 全绿 | `tests-data/` 只放了 13 个"能过"的文件；失败的那些**从未进入语料**。`corpus.rs` 的 `CORPUS` 常量（`:22-36`）是手挑的，没有"跑遍 fo-dicom 全部测试数据"的机制 |
| 2 个字符集文件**直接 panic**（`chrI2`、`chrKoreanMulti`） | 全绿 | 同样不在语料内；`tests/fuzz.rs` 的随机字节扫描未覆盖这类合法-but-少见的结构 |
| JSON group length 未剥离 | 全绿 | golden 自生成（§4.3 #3） |
| 4 个 A-ASSOCIATE 子项线格错误 | 全绿 | 12 个自往返测试（协议组评审结论）；本报告的 `pdu_goldens_are_current` 也属同一类 |
| `ISO_IR 109/110/144` 等映射成 Windows 代码页，俄文/阿拉伯文乱码 | 全绿 | 语料只有 GB2312/ISO-2022-JP；`charset.rs:437` 把错误映射断言成期望值 |

这张表是全报告最有说服力的一段：**"测试全绿"与"实现正确"在本仓库里是两件几乎无关的事**。修复测试体系比修复任何单个缺陷都更紧急——否则同样的缺陷会以同样的方式再次通过。

与 `docs/development/03-testing.md` 自评的偏差（本文实测）：

| 文档声称 | 实际 |
| --- | --- |
| `:24` "`fuzz/` # cargo-fuzz targets (reader, pdu)" | **`fuzz/` 目录不存在**（`Test-Path fuzz` = False）；`:88-89` 的 `fuzz/reader`、`fuzz/pdu` 目标不存在 |
| `:13`/`:69` "nightly CI 对 DCMTK、fo-dicom 做外部 interop" | 唯一入口 `interop.rs:143` 在未设 `RADICOM_PEER` 时 `eprintln!` 后 `return`，**不是 `#[ignore]`**，CI 里永远绿 |
| `:61` "byte-level round-trip: 与原文件字节相等" | `corpus.rs:60-63` 比较的是**两次由 radicom 自己写出的字节**（`first == second`），从未与原文件比较；实测 13 个语料文件全部 `first != original` |
| `:54-59` "golden 由 fo-dicom 输出" | `xtask/src/main.rs:446-458` 用 **radicom 自己的** `to_dump_string`/`dataset_to_json` 生成；`network/tests/goldens.rs:36-53` 还**重复实现**了 fixture 构造（注释"keep in sync with regen_goldens"），两处无共享源 |
| `docs/08:160-166` §8 "Test Requirements Checklist" | 7 项**全部未勾选**（`- [ ]` × 7，`- [x]` × 0），但其中 JSON/SR/DICOMDIR/Dump 的实际测试条数并不少 |

### 4.2 恒真 / 自我实现测试

判据：断言的对象是测试自己刚构造的值、常量与自身字面量比较、或断言永不可能失败。

#### (a) 断言"错误的常量"——把缺陷固化成期望值（最危险的一类）

| 测试 | 位置 | 为什么恒真 |
| --- | --- | --- |
| `a_short_code_writes_the_short_tag_family` | `sr/code_item.rs:130-138` | 断言 `ds.try_get_str(&SHORT.value) == Some("T-A0100")`，而 `SHORT.value` 就是 `(0018,0090)`——被写进去的那个错误 tag。**修好 §2.2 会让它变红** |
| `a_long_code_writes_the_long_tag_family` | `sr/code_item.rs:140-147` | 同上，断言 `LONG.value`；而 `LONG.scheme_version` 是错的 `(0008,0106)` 却从未被断言 |
| `a_urn_forces_the_long_family_even_when_short` | `sr/code_item.rs:149-155` | 断言 `LONG.value` 含 `urn:oid:1.2.3`；实际应写 `(0008,0120) URNCodeValue`/UR，测试把错误行为钉住 |
| `a_code_round_trips_through_a_dataset` | `sr/code_item.rs:157-164` | `write_into` 与 `read_from` 用**同一对错误常量**，写读互相印证。这是"自洽往返"，不是"符合标准" |

#### (b) 用自己写出的字节验证自己写出的 offset

| 测试 | 位置 | 为什么恒真 |
| --- | --- | --- |
| `record_offsets_are_absolute_positions_of_item_tags` | `media/mod.rs:783-811` | 扫描 `dir.to_bytes()` 找出所有 item 头，再断言 `first_root == item_positions[0]`。**只用 1 条文件、无 icon**，恰好绕开 §2.1；且 `item_offsets` 与断言用同一种"找 FFFE,E000"逻辑 |
| `written_offsets_are_real_byte_positions` | `media/mod.rs:890-925` | 同样扫自己写出的字节，断言"每个 offset 都是某个 item 头位置"。带 icon 时错位的 436 **也是一个真实 item 头**，所以即使加了 icon 这条也照样绿——它验证的是"是个数"，不是"是这个记录" |
| `written_offsets_point_at_the_next_sibling_and_first_child` | `media/mod.rs:957-971` | 只断言 `assert_ne!(offset, 0)`，不检查指向的是**哪条**记录 |
| `add_file_with_icon_attaches_a_preview_that_survives_save` | `media/mod.rs:928-954` | 只验证 icon 序列存在且 round-trip 存活；**完全没有验证 offsets**，所以 §2.1 在"有 icon 的场景"里零覆盖 |

#### (c) 常量与自身字面量比较

| 测试 | 位置 | 为什么恒真 |
| --- | --- | --- |
| `target_constants_are_stable` | `logging.rs:94-100` | `assert_eq!(targets::NETWORK, "radicom::network")` —— 两者是同一份字面量的两个副本。它只能发现有人改了常量字符串，而这是编译器能发现的重命名 |
| `dictionary_version_is_reported` | `crates/radicom-dict/src/lib.rs:59-61` | `assert_eq!(DICTIONARY_VERSION, "2026b")` —— 常量对自己的字面量 |
| `uid_table_shape` | `radicom-dict/src/lib.rs:50-56` | 除了 `VERIFICATION` 常量对字面量，还有 `UIDS.len() == 1928`（见下） |

#### (d) 纯表形状计数（不是行为测试）

`crates/radicom-dict/src/lib.rs` 的 4 个测试里有 3 个是"生成常量等于某个数字"：

- `public_dictionary_table_shape`（`:16-38`）：`exact + masked == 5347`
- `private_dictionary_table_shape`（`:41-47`）：`rows == 4678`、`creators.len() == 235`
- `uid_table_shape`（`:50-56`）：`UIDS.len() == 1928`

这些是 **golden count**：`xtask gen-dict` 输出的任何变化都会让它们变红，但它们**不验证任何一次查找行为**（除了 `public_dictionary_table_shape` 里顺手做的 `binary_search` 找 PixelData + 找 OverlayRows，那两条是真测试）。真正的验证——"`(0008,0060)` 查得到 `Modality`/`CS`"、"私有 creator 的家族能解出"——在 `radicom/src/model/dictionary.rs` 的 7 个测试里，不在 dict crate 这里。

#### (e) 断言自己刚写入的值

| 测试 | 位置 | 说明 |
| --- | --- | --- |
| `a_dump_lists_tag_vr_keyword_and_value` | `dump/mod.rs:181-188` | `sample_dataset()` 自己 `insert_str(PATIENT_NAME,"DOE^JOHN")`，再断言 dump 含 `"DOE^JOHN"`。判别力弱但不为零（能发现 dump 完全不输出值），属于"格式嗅探"而非"内容验证" |
| `long_values_are_truncated_to_the_limit` | `dump/mod.rs:198-207` | 构造 100 个 `A`，断言 `!out.contains(&"A".repeat(20))`。若 dump 把所有值都丢掉也会通过——**这是"截断测试"的经典假绿形态** |
| `an_empty_value_is_labelled` | `dump/mod.rs:224-230` | 插空串、断言含 `(empty)`；判别力尚可（`(empty)` 是真实分支） |

**stub 验证（已做）**：在临时副本里把 `dump_dataset` 换成返回一个常量字符串（含 `PatientName`/`SQ`/`item #0`/`(empty)`/`PRIVATE`/`0 1 2 3`/`32 bytes`/`ReferencedImageSequence` 等所有被断言的字面量），**8 个 dump 测试全部通过**。这证明 `dump/mod.rs` 的整组测试只验证"输出里有这些词"，不验证结构、不验证行数、不验证层级——与 `goldens.rs` 的 dump golden 相比，后者才是真验证（但 golden 又是自生成的，见 §4.3）。

#### (f) 完全无断言的测试（8 个）

| 测试 | 位置 |
| --- | --- |
| `write_to_log_does_not_panic` | `logging.rs:87-91` |
| `malformed_inputs_do_not_panic` | `io/file.rs:764-...`（`let _ = DicomFile::from_bytes(...)`） |
| `arbitrary_bytes_never_panic` | `tests/fuzz.rs:38-55` |
| `mutated_valid_file_never_panics` | `tests/fuzz.rs:57-70` |
| `the_noop_collector_is_a_safe_default` | `radicom-network/src/observability.rs:333-339` |
| `random_bytes_do_not_panic` | `radicom-network/src/pdu.rs:1094` |
| `loopback_abort_ends_the_connection` | `radicom-network/src/server.rs:1901-1923` |
| `arbitrary_pdu_bytes_never_panic` | `radicom-network/tests/fuzz.rs:23` |

这 8 个是**有意的** smoke test（"不 panic"本身是验收标准，"必须返回 typed `Result`"），单独看合理。问题在于它们是**唯一**的鲁棒性覆盖：`docs/09:88-89` 承诺的 `fuzz/reader` / `fuzz/pdu` cargo-fuzz 目标不存在，所以"没有发现 panic"的证据强度远低于文档宣称。

#### (g) 只断言 `is_ok()` / `is_some()`

| 测试/断言 | 位置 |
| --- | --- |
| `assert!(identity.validate().is_ok())` | `radicom-network/src/pdu.rs:1116` |
| `assert!(options.client_config().is_ok())` | `radicom-network/src/tls.rs:208` |
| `assert!(...is_some())` / `assert!(...is_none())` | `radicom-network/src/engine.rs:354`、`:361` |
| `assert!(json_to_dataset(&json, ...).is_ok())` | `radicom-json/src/lib.rs:808`（`indented_output_is_readable` 只检查能读回，不检查读回内容） |
| `assert!(Uid::is_valid(&new_uid))` | `rules/transform.rs:363`——只验证格式，`assert_ne!(new_uid,"1.2.3")` 在上一行提供了最小判别力 |

### 4.3 假绿（实现被 stub 掉仍然通过）

判据：把被测实现改成"永远返回成功/空值/常量"后测试仍然绿。

#### 高价值：已实测确认

| # | 测试 | 位置 | 假绿机制 | 实测结果 |
| --- | --- | --- | --- | --- |
| 1 | `every_corpus_file_write_is_stable` | `tests/corpus.rs:50-65` | `assert_eq!(first, second)`，`first` 和 `second` **都是 radicom 写出来的**。这是"找不动点"，不是"与原文件一致"。把 writer 改成静默丢字段，只要丢得**稳定**，本条不红 | 在临时副本把 writer 的 `(0008,1030)` 分支改成"丢弃"后：本条 **ok**、`radicom` dump golden **ok**、`radicom-json` golden **ok**；只有 `every_corpus_file_round_trips_semantically` 红（且只因 `1403-OB.dcm` 带该 tag）。实测 13 个语料文件 `first != original_bytes`，首个差异字节恒在 **140**（file meta 长度不同） |
| 2 | `pdu_goldens_are_current` | `radicom-network/tests/goldens.rs:66-70` | golden 由 `xtask/src/main.rs:462` 调用**被测的** `encode()` 生成；且 fixture 构造在测试里被**复制一份**（`:36-53`，注释 "keep in sync"）。encode 写错 → golden 与测试一起错 | 结构上无法区分"正确"与"自洽"。本次用解码验证：golden 解出的 `called_ae="RADICOM_SCP"`、`calling_ae="RADICOM_SCU"`、`protocol_version=1`——**数值正确**，但这只说明当前值可信，不说明测试有判别力 |
| 3 | `json_golden_is_current` | `radicom-json/tests/goldens.rs:15-23` | 同上：`xtask:457` 用被测的 `dataset_to_json` 生成 golden。§2.3 的 group length 缺陷**正被这个 golden 固化**（golden 里 65 个键、含 6 个 `*0000`） | golden 文件本身就是缺陷快照 |
| 4 | 全部 8 个 `dump/mod.rs` 测试 | `dump/mod.rs:181-260` | 见 §4.2(e)，只嗅探字面量 | stub `dump_dataset` 后 **8/8 通过** |
| 5 | `a_short_code_writes_the_short_tag_family` 等 4 个 | `sr/code_item.rs:130-164` | 见 §4.2(a)，断言错误常量 | 修正确实现 → 测试红 |
| 6 | `the_builtin_profile_parses_and_is_large` | `anonymizer/profile.rs:275-279` | `assert!(profile.rows().len() > 600)`——只数行数。把每行的 `actions` 全填 `None`（即**所有 tag 都不动作**）这条依然绿 | 未实测（结构显然），但 §2.4 的 CleanGraph 全失效正是这种"行数够多就算通过"的后果 |
| 7 | 全部 4 个 `radicom-dict` 测试 | `radicom-dict/src/lib.rs:13-62` | 表形状计数，见 §4.2(d) | 把 `DICTIONARY` 的每个 `vr` 字段全改成 `"ZZ"`，4 个测试里 3 个仍绿（只有断言 `pixel.vr == "OB\\OW"` 的那条红） |
| 8 | `external_peer_c_echo_and_optional_c_store` | `radicom-network/tests/interop.rs:142-163` | 未设 `RADICOM_PEER` 时 `eprintln!` + `return` → **通过**。不是 `#[ignore]` | CI 里 `interop` 报 "3 passed"，实际外部互操作覆盖 **0** |

#### 中价值：结构推断（未逐条 stub）

| # | 测试 | 位置 | 假绿机制 |
| --- | --- | --- | --- |
| 9 | `charset` 三个语料被跳过 | `tests/corpus.rs:74-76` | `if category.starts_with("charsets") { continue; }`，理由是"PN 重编码可能改变内存表示"。**实测推翻该理由**：不跳过时 `chrGB2312` / `chrJapMulti` / `chrSQEncoding` 三个文件 `semantic_equal = true`（`strip_group_lengths` 后）。这个 `continue` 掩盖的是不存在的问题，却让人以为 charset 已被覆盖 |
| 10 | `every_corpus_file_parses` | `tests/corpus.rs:38-48` | 只断言 `!file.dataset.is_empty()`。"解析出 1 个元素"和"解析出 65 个正确元素"都通过 |
| 11 | `a_real_sr_document_walks_its_content_tree` | `tests/corpus.rs:128-164` | 断言 5 个根、28 个节点、含 Container/Text/Image。这些是**真断言**（改坏 SR 解析会红），但不检查任何节点语义（code 值、relationship、measured value），所以 §2.2 的 tag 错误完全不影响它 |
| 12 | `dumping_a_written_file_shows_header_and_pixels` | `tests/cli_dump.rs:33-53` | 同样的"嗅探字面量"模式（`contains("PatientName")`、`contains("DOE^JANE")`），且 `assert!(!syntax.is_empty())` 几乎恒真 |
| 13 | 字符集整组测试 | `encoding/charset.rs:405-500`（13 个测试） | 参见 §2.11。这些测试全部用 `charset_encoding()` 的返回值与**另一处同样错误的映射**互相印证（`:437` `charset_for_encoding(windows-1251) == Some("ISO_IR 110")`）。把 10 个 `ISO_IR` 全改成任意别的代码页，这 13 个测试仍有 12 个通过（只有显式断言 `.name()` 的那条会红）。它们测的是"编码值能往返"，不是"编解码符合 PS3.3 C.12.1.1.2" |

#### 需要说明的一个"不是假绿"

`self_interop_c_echo_over_tcp` / `self_interop_c_store_real_file_round_trips`（`interop.rs:51/67`）是**真测试**：`DicomServer::bind("127.0.0.1:0")` + `accept_one()` + `DicomClient` 走真 TCP，C-STORE 后比对 dataset（`strip_group_lengths` 之后）。它们能发现真实的协议/传输回归。§4.5 详述。

### 4.4 覆盖缺口

把 `docs/development/02-feature-list.md` 的 feature ID 与测试交叉核对（grep 每个 ID 在 `crates/` 下的出现次数，再逐个确认那处是测试还是注释）。以下 22 项是**文档声称有、测试实际没有**的最重要缺口：

| # | 缺口 | 文档依据 | 现状 |
| --- | --- | --- | --- |
| 1 | DICOMDIR 带 icon 时的 offset 正确性 | `docs/08:162` "byte-verify offsets against fo-dicom-produced DICOMDIR" | 零覆盖；§2.1 因此漏网 |
| 2 | DICOMDIR 与 fo-dicom 产物的交叉验证 | `docs/08:162` | 零覆盖；所有 offset 测试都对着自己 |
| 3 | `nearest-offset` 容忍度 | `docs/08:162`、`docs/01:170` | `a_corrupt_lower_offset_is_tolerated`（`media/mod.rs:1040`）只测"越界 offset 变成根"，不测"最近邻 offset" |
| 4 | 图标 builder 输出校验（尺寸/位数/长度） | `docs/08:162` | `icon.rs` 8 个测试测了"无 icon"的各种情形，但没有一条验证生成 item 的 `Rows/Columns ≤ 128`、`BitsAllocated == 8`、`PixelData` 长度 == `Rows*Columns` |
| 5 | `FileSetID` 空值被拒 | `docs/08:71` | 零覆盖 |
| 6 | 扫描器的 `Stop()` / 完成事件 / 预统计 | `docs/01:172`（MEDIA-03） | 已实现（`Stop`/`Complete`/`Progress` 事件），已有测试 |
| 7 | SR：真实文档的 code 值/relationship/measured value | `docs/08:163` | `a_real_sr_document_walks_its_content_tree` 只数节点 |
| 8 | SR：type-checked 构造矩阵（15 ValueType × 7 Relationship 全组合） | `docs/08:163` | `content_item.rs` 有 `value_types_round_trip`/`relationships_round_trip` 覆盖枚举，但没有"每个 ValueType 的专用构造器写对 tag"的矩阵；`typed_scalar_items_write_their_value_tag`（`:525`）只覆盖部分 |
| 9 | SR：`save` 后重新打开（explicit length 序列） | `docs/08:163` | `save_writes_a_file_that_opens_back`（`structured_report.rs:172`）**已覆盖**——此项不算缺口，保留在表中以示核对过程 |
| 10 | SR：`CodeItem` 与标准 tag 的一致性 | `docs/01:174`（SR-02） | 零覆盖；测试反而锁住错误值 |
| 11 | 匿名化：跨调用 UID 一致性 | `docs/08:165` "UID consistency across calls" | **零覆盖**，而功能本身也是错的（§2.5） |
| 12 | 匿名化：每个选项下每个 tag 的动作采样 | `docs/08:165` "assert per-tag actions (sampled)" | `profile.rs` 只采样了 4 个 tag、且都在 `BasicProfile` 一个选项下 |
| 13 | 匿名化：对全语料跑完整 profile | `docs/08:165` | 零覆盖；`anonymizer/mod.rs` 的 14 个测试全用合成小数据集 |
| 14 | 匿名化：clone-vs-inplace 等价 | `docs/08:165` | 零覆盖（`anonymize` 与 `anonymize_in_place` 的关系无测试） |
| 15 | 匿名化：自定义 CSV 加载 | `docs/08:165` "custom CSV profile load" | `profile.rs:315` 只测"空 profile 被拒"，没测"合法自定义 CSV 生效" |
| 16 | JSON：与 fo-dicom 输出的 golden 对比 | `docs/08:160` "golden files from fo-dicom output" | golden 是自生成的（§4.3 #3） |
| 17 | JSON：group length 被剥离 | `docs/01:167` | 零覆盖，且功能错误（§2.3） |
| 18 | JSON：`tags_as_keywords` 选项 | `lib.rs:70` | 该选项在**整个 repo 里从未被置为 `true`**（grep 无 `tags_as_keywords: true`），零覆盖 |
| 19 | JSON：`validate_on_read` 选项 | `lib.rs:72` | 已实现：`json_to_dataset` 现在按 `options.validate_on_read` 读后校验，并有测试 |
| 20 | JSON：空 SQ / 空值组件的 fo-dicom 兼容 | PS3.18 Annex F | 零覆盖；§2.7 的 a/b 两项因此全是"自洽往返" |
| 21 | XML：`privateCreator` 属性、片段序列 | `docs/08:161`（"golden export snapshot tests"） | `xml.rs` 11 个测试没有一条涉及 `privateCreator` 或 `write_fragments`（grep 无 `privateCreator`、无 `insert_fragments`） |
| 22 | Dump：与 `FO-DICOM.Dump` 的格式对齐 | `docs/01:202`（INF-04 "mirroring FO-DICOM.Dump"） | `dump/mod.rs` 的 8 个测试只断言"包含某词"，没有一条对比 fo-dicom 的实际行格式（`tag VR [value] #len, keyword`）；radicom 的格式与 fo-dicom 根本不同 |
| 23 | 字符集：ISO-8859-3/4/5/6/7/8/9 的**正确**解码 | PS3.3 C.12.1.1.2、`docs/01` ENC-01/02 | 语料只有 GB2312/ISO-2022-JP；10 个 `ISO_IR` 映射错误（§2.11）零覆盖。Lead 的 `chrRuss`/`chrArab` 一旦进入语料会立即暴露 |
| 24 | 语料完整性：fo-dicom 全量测试数据的解析覆盖 | `docs/09` §3（INF-06 "Test data organized after fo-dicom Tests"） | 语料是**手挑的 13 个能过的文件**；Lead 实证有 8 个 fo-dicom 语料在 radicom 解析失败、2 个字符集文件 panic，全部不在语料内 |
| 25 | 编码→解码只能证明自洽，不能证明符合标准 | — | 这是**方法论缺口**而非单个用例：全仓库没有一条"用第三方 oracle（fo-dicom 输出 / DCMTK 产物）做断言"的测试。`pdu_goldens`、`GH178.json`、`GH178.dump.txt` 全部由被测函数自己生成 |

另外两个**跨 crate 的结构性缺口**：

- **`xtask` 零测试**（507 行）：`gen-dict`（会重写 5347 条字典）与 `regen-goldens`（会重写 5 个 golden）都没有任何测试。`regen-goldens` 还是 golden 的唯一来源，一旦它自己写错，golden 与测试一起错。
- **feature 组合未测**：`--all-features` 比默认多跑 **125 个 `radicom` 测试 + 9 个 network 测试**（497-372、248-239）。默认 `cargo test` 不覆盖 `render`/`anonymize`/`tls`/`otel` 下的任何测试；CI 若只跑默认 feature（大多数项目如此），这些测试长期不被执行。

### 4.5 具体验证过的重点

#### (a) `crates/radicom/tests/corpus.rs` 是否真的往返？

**部分真、部分空**。逐条结论：

- `every_corpus_file_round_trips_semantically`（`:68-104`）**是真的**，而且是最有价值的测试：parse → write → re-parse → `Dataset` 深比较（`model/dataset.rs:751` 的 `PartialEq` 是逐 item 递归比较，排除 transfer syntax 与 validate 开关）。它能发现字段丢失——我用"丢弃 `(0008,1030)`"的 stub 验证过，只有它会红。
- `every_corpus_file_write_is_stable`（`:50-65`）**是固定点检查，不是往返**。`first`/`second` 都是 radicom 产出。实测 13 个文件 `first != original`（首个差异字节恒为 140，即 file meta 长度），所以它无法回答"是否与原文件一致"。文档 `docs/09:61` 把它当作 byte-level baseline 是错的。
- `every_corpus_file_parses`（`:38-48`）只断言非空，判别力接近于零。
- `chronic skip`：charset 三个文件被 `continue` 掉（`:74-76`），但**实测不跳过也会通过**（三个都 `semantic_equal=true`），所以这个 skip 是多余的防御，反而制造了盲区。
- `strip_group_lengths`（`:167-191`）本身正确（递归处理嵌套 SQ）。它的存在证明 writer **确实**会丢 group length（实测 GH178：原 6 个 `*0000` 元素，写出后重解析为 0 个），这与 §2.3 的 JSON 侧不丢形成口径矛盾。

#### (b) golden 是否比较有意义的字节？

**两处有意义、两处自生成**：

| golden | 生成者 | 判别力 |
| --- | --- | --- |
| `tests-data/goldens/GH178.dump.txt` | `xtask:450` 调 `file.dataset.to_dump_string()`（被测函数） | 只能发现漂移。但 dump 的**内容**是可人工审阅的（152 行，含具体值），所以 review 时有意义 |
| `tests-data/goldens/dicomdir.dump.txt` | 同上 | 765 行、1 个 SQ、80 个 item、`(0004,1430)` × 80、**嵌套深度只有一层**（`^\s{6,}` 匹配 0 行）。也就是说这个 golden **验证不了** DICOMDIR 的树结构，只能验证"80 条记录平铺输出" |
| `tests-data/goldens/GH178.json` | `xtask:457` 调 `dataset_to_json`（被测函数） | 固化缺陷（含 6 个 group-length 键，§2.3） |
| `tests-data/goldens/{associate_rq,pdata_cecho}.hex` | `xtask:462` 调 `encode`（被测函数）+ 测试内重复实现的 fixture | 解出的字段值经我手工核对**正确**（`RADICOM_SCP`/`RADICOM_SCU`/协议版本 1/PDU 长度 160 与实际 166 相符），但测试本身没有独立的 oracle |

#### (c) `interop.rs` 是否真的用 TCP 环回？外部对端测试是否静默跳过？

- `self_interop_c_echo_over_tcp`（`:51`）与 `self_interop_c_store_real_file_round_trips`（`:67`）**是真环回**：`DicomServer::bind("127.0.0.1:0", ...)` 取随机端口 → `tokio::spawn(server.accept_one())` → `DicomClient` 连同一端口 → C-ECHO / C-STORE → 比对。无 mock、无内存短路。
- `external_peer_c_echo_and_optional_c_store`（`:142-163`）**会静默跳过**：`let Ok(peer) = std::env::var("RADICOM_PEER") else { eprintln!("skip: ..."); return; };`。因为不是 `#[ignore]`，`cargo test` 会把它算作 **passed**——`interop` suite 显示 "3 passed"，而其中 1 个什么都没做。这是 §4.3 #8。

#### (d) 附带发现：默认 A-ASSOCIATE-RQ 不携带 ImplementationClassUID（不在我的 scope 内，转交网络评审）

构造 golden 的 `fixture_associate_rq()` 用的是裸 `Associate::new(...)`（`network/tests/goldens.rs:38`、`xtask:486`），其 `user_information` 是 `UserInformation::new(16384)`，`implementation_class_uid = None`。解码 golden 可确认 `implementation_class_uid: None`。

`dicom-network/src/server.rs:199` 与 `:1048` 在真实路径上都会显式设置它，所以**生产路径没问题**；但 `association.rs:400` 的 `build_user_information` 在整个 crate 里**没有任何调用点**（`Select-String build_user_information` 只有定义 + 它自己的测试），而它的测试 `user_information_carries_identity_and_roles`（`:679`）是这条"死代码"唯一的覆盖。这是"测试覆盖了一个没人调用的函数"的典型案例，转交网络评审确认。

---

## 5. 给 Lead 的修复优先级建议

按"修完后能一次性消掉最多测试盲区"排序：

1. **测试基础设施（最高优先，因为它决定其余修复是否会被回归）**——(i) 把 `external_peer_*` 改成 `#[ignore]`，让 CI 的 "passed" 不再虚报 (§4.3 #8)；(ii) 引入**第三方 oracle**：至少把 `tests-data/goldens/GH178.json` 换成一份真实 fo-dicom 输出、给 PDU golden 加 fo-dicom 抓包对照，这是唯一能突破"自往返只能证自洽"的办法（§4.4 #25）；(iii) 把 fo-dicom 全量语料纳入 `corpus.rs`，含当前解析失败的 8 个与 panic 的 2 个；(iv) 把 `corpus.rs:60-63` 的 `first == second` 改为与原文件对比（或明确改名 fixed-point 并说明它不证明符合规范）；(v) 删掉 `corpus.rs:74-76` 的 charset `continue`（实测不需要）。
2. **§2.11 字符集映射**（高）——10 个 `ISO_IR` 改为 ISO-8859-x，同时**重写** `charset.rs:437` 那条把错误映射当期望值的断言，并把 `chrRuss`/`chrArab` 加入语料。改动跨 crate，但影响面最大（任何非拉丁文本的读写）。
3. **§2.2 `CodeItem` 的 tag 族**（高）——修代码 + 同时重写 `code_item.rs` 的 4 个测试。必须一起改，否则测试会挡路。
4. **§2.1 DICOMDIR 带 icon 的 offset**（高）——修 `item_offsets`/`rebuild`，并补一个"带 icon 后 `from_bytes` 的树与 `add_file` 前的树相同"的测试（现在的 3 个 offset 测试都证明不了这件事）。
5. **§2.3 JSON group length**（高）——加过滤 + 重生成 golden + 删掉 `corpus.rs` 里对 JSON 的隐含期待。
6. **§2.5 匿名化 UID map 提升到实例**（中）——这是 `docs/08:165` 明确列出、却完全没有测试的验收项。
7. **§2.4 `action_for` 按选项建表**（中）——修完补"同一 tag 在两个选项下动作不同"的测试。
8. 其余中/低项（§2.6、§2.7、§2.8、§2.9、§2.10、§2.12、§3.x）可随相关模块的下一轮改动一并处理。

**关于 `docs/`**：`docs/development/03-testing.md` 的 §1/§5/§7 与 §4 对测试强度的描述与实测不符（`fuzz/` 目录不存在、外部 interop 恒绿、byte-level baseline 是固定点检查），`docs/08:160-166` 的 §8 checklist 全未勾选但部分项其实已有测试，`docs/08:91`/`docs/01:174` 把错误的 code-item tag 族写成已实现。建议本轮之后统一校正，否则后续评审仍会被这些自评误导。

---

## 6. 与 Lead 差分测试结论的逐条对照

Lead 用本地编译的 fo-dicom 给出了 5 项结论。本文的核对结果：

| Lead 结论 | 本文核对 | 说明 |
| --- | --- | --- |
| (1) JSON group length 未剥离（`lib.rs:109-123`） | **确认** | 独立复现：GH178 顶层 65 键含 6 个 `*0000`、嵌套 item 也带；`docs/01:167` 承诺剥离但未实现（§2.3） |
| (2) 空 SQ 写成 `{"vr":"SQ","Value":[]}`，fo-dicom 写 `{"vr":"SQ"}` | **确认，且补充一个更严重的读取侧缺陷** | 写出侧见 §2.7a；**读取侧**：`lib.rs:419` 把 fo-dicom 形状 `{"vr":"SQ"}` 读成 `DicomElement::new_empty(tag, Vr::Sq)`（VR 为 SQ 的**普通 element**），而非空 `DicomSequence`；只有 radicom 自己的形状才读成 sequence。自测两个形状都"往返一致"，所以测试全绿 |
| (3) CS 空首值写 `""` 而非 `null` | **确认，且读出也不对称** | `lib.rs:214`、`:503-509`；补：`value_to_text:513` 把 `Value::Null` 读回 `String::new()`，故读 fo-dicom 的 `[null,"ISO 2022 IR 58"]` 会得到 `"\\ISO 2022 IR 58"`，**往返不保持**（§2.7b） |
| (4) IS `"0001"` 保持字符串 vs fo-dicom 输出 `1` | **确认，并定性为"可辩护但必须显式声明"** | `lib.rs:304-313` 的 `PreferablyAsNumber` 因 `format_number_like(1.0) != "0001"` 拒绝编号；fo-dicom 默认 `AsNumber`（`JsonDicomConverter.cs:69`）。建议保留 radicom 默认（更安全），但写进文档并补 `"0001"` 的显式断言（§2.7c） |
| (5) 正向：1403-OB、TestPattern_Palette 语义一致；DICOMDIR、PN、InlineBinary、bulk 均匹配 | **接受，与本文独立结论一致** | 本文在 clean repo 实测 `1403-OB.dcm` 往返 `orig=45 written=45 equal=true`；`corpus.rs` 的 `every_corpus_file_round_trips_semantically` 也覆盖它 |
| (6) DICOMDIR icon offset：Lead 独立构建（`[372,440,510,592]` / `[372,440,510,592,736]`，`first_root` 372 → 440） | **确认，数字一致** | 我的构建是 `[368,436,506,578]` / `[368,436,506,578,722]`，`first_root` 368 → 436。绝对偏移因 FileSetID/SOPInstanceUID 长度不同而异，但**错位规律相同**：根指针恰好下移一个记录（PATIENT → STUDY）。已补充"只在 record sequence 内嵌序列时触发"这一关键限定（§2.1），这正是无 icon 的 3 个 offset 测试全绿的原因 |
| (7) `CodeItem` 是**三个**独立缺陷，且 core 前提错误；`(0018,0090)` 是 DS 所以产出**非法**而非仅无意义；LONG 的 `(0008,0106)` 是 DT；`LongCodeValue`/`URNCodeValue` 完全未实现 | **全部确认，并逐条实测** | (a) `(0018,0090)` 实测 `VR=DS`（`DataCollectionDiameter`），写 `"T-A0100"` 进 DS 槽 → 违规数据集。(b) `(0008,0106)` 实测 `VR=DT`（`ContextGroupVersion`），radicom 把 version `"3"` 以 SH 写入。(c) 17 字符值实测写进 `(0008,0100)`(SH,≤16) → 长度越界；URN 同样写进 `(0008,0100)`。字典里 `(0008,0119)`=UC、`(0008,0120)`=UR **存在但从未被引用**。唯一与 Lead 表述的差异：`(0018,0100/0102/0104)` 在 radicom 字典里是**完全缺失**（keyword 回落 `Unknown`），不是登记为 `Unknown` 的行——结论相同（非法 tag），已按实测写法记录（§2.2 a/b/c） |
| (8) 匿名化 `(0008,1140)` 整条删除**不是缺陷** | **接受，并已写入 §2.0** | PS3.15 Table E.1-1 标 X；Lead 的 fo-dicom 6.0 对照结果一致；`ps3.15.csv` 与 fo-dicom `DefaultProfile` 逐字节相同（655 行），故本报告不审查表内容 |
| (9) 8 个 fo-dicom 语料解析失败 + 2 个字符集文件 panic | **接受，作为 §4.1 的核心证据引用** | 这些文件不在 `tests-data/`（只有 13 个手挑文件），故 radicom 全绿。已据此新增覆盖缺口 #24 |
| (10) 4 个 A-ASSOCIATE 子项线格错误 + 12 个自往返测试仍绿 | **接受（属协议组 scope）** | 本文 §4.5(d) 独立发现同源的实例：`build_user_information`（`association.rs:400`）无任何调用点，只被自己的测试覆盖 |
| (11) (a) PDU golden 是否比较真实字节？ (b) `write_is_stable` 是否自参照？ | **确认 Lead 的判断 (b) 成立；(a) 的 golden 是真实字节但 oracle 自生成** | (b) `corpus.rs:60-63` 的 `first == second` 两侧都由 radicom 产生，实测 13 个文件 `first != original`（首差恒为字节 140）。(a) `pdu_goldens` 比的是真实 hex 字节，但 golden 由 `xtask:462` 调被测 `encode()` 生成，因此只能测漂移；我另行解码核对了内容（`RADICOM_SCP`/`RADICOM_SCU`/版本 1/长度 160），数值本身正确 |
| (12) 字符集 ISO_IR 109/110/126/127/138/144/148 映射成 Windows 代码页 | **确认，并给出具体错值** | `charset.rs:19-29`；实测 `ISO_IR 109` + `E8 E9` → `"θι"`（应 `"èù"`）、`ISO_IR 110` + `C8 D8` → `"ИШ"`（应 `"ČØ"`）、`ISO_IR 144` + `B0 B1` → `"°±"`（应 `"АБ"`）。fo-dicom `DicomEncoding.cs:151/152/156` 用 `iso-8859-3/4/5`（§2.11） |
| (13) `PS3.15 CSV` 需要审查吗 | **不需要** | 已与 fo-dicom `DefaultProfile` 逐字节比对相同；§2.4 指出的是 `action_for` 的**查表算法**，与表数据无关 |

**本文与 Lead 结论无冲突。** 唯一的补充是 (2) 的读取侧缺陷（fo-dicom 形状的空 SQ 被读成 element 而非 sequence）和 (3) 的读出不对称，这两点 Lead 未测，我通过 `json_to_dataset(r#"{"00081111":{"vr":"SQ"}}"#)` 与 `#[null,"ISO 2022 IR 58"]` 的往返实测确认。

**关于 `docs/`**：`docs/development/03-testing.md` 的 §1/§5/§7 与 §4 对测试强度的描述与实测不符（`fuzz/` 目录不存在、外部 interop 恒绿、byte-level baseline 是固定点检查），`docs/08:160-166` 的 §8 checklist 全未勾选但部分项其实已有测试。建议本轮之后统一校正，否则后续评审仍会被这些自评误导。
