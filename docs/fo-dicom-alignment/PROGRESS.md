# fo-dicom 对齐：修复进度（滚动更新）

本文件记录 `docs/fo-dicom-alignment/` 五份走查报告各条目的当前处置状态，供后续轮次继续开发时
直接对照，避免重复审查。代码是唯一事实来源：状态以当前 `HEAD` 的代码与测试为准，不以报告当时
的结论为准（报告是快照，见各文件顶部说明）。

更新方式：每完成一个条目，补上对应提交/测试名；发现新差异时在“待办”追加一行。

## 已验证完成（P0 / P1 / P2 核心）

| 报告条目 | 状态 | 证据（测试 / 符号 / 提交） |
| --- | --- | --- |
| 01 §2.1 片段分隔符长度未消费 | 已修复 | `parser` 测试；`io/reader.rs` |
| 01 §2.2 私有序列探测 / 01 §2.5 私有 tag VR 恒 UN | 已修复 | `2a5ce72`、`reader.rs` `IsPrivateSequence` 注释与探测 |
| 01 §2.3/§2.4 ISO_IR 字符集映射与韩文溢出 | 已修复 | `8db55fc`；`charset.rs` 使用 iso-8859-*；`chrRuss`/`chrArab` 语料 |
| 01 §2.5b CP-246 条件反转 | 已修复 | `8db55fc`；`GH177` 语料 |
| 01 §2.5c `Suspended`/`IsPartial` | 已修复 | `53ae9e6`、`7c93c0a`；`file.is_partial`；`corpus.rs` |
| 01 §2.6 孤立 item tag `(FFFE,E000)` | 已修复 | `53ae9e6`；`reader.rs` |
| 01 §2.7 `FileWindow` 真正按区间读 | 已修复 | `buffer.rs` `file_window_reads_lazily_and_in_ranges` |
| 01 §2.10 校验规则收紧（DA/DS/IS） | 已修复 | `validation/mod.rs` 严格解析 |
| 01 Tag.ToString("G"/"X"/"g") | 已实现 | `0e51b55`；`tag.rs` `the_fo_dicom_format_variants_are_available` |
| 01/exec `DicomDatasetComparer` 可配置语义 | 已实现 | `Dataset::content_equals`；`content_comparison_skips_unresolved_bulk_data` |
| 01 写侧 `LargeObjectSize` | 已实现 | `WriteOptions::large_object_size`；`large_object_size_controls_chunking...` |
| 01 解析器全局锁 / 每元素 seek（P-1/P-2） | 已修复 | `94619fe`、`dd8bac1`、`b8533cd` |
| 02 §2.1 `width/height` 用 u32 读 US | 已修复 | `8db55fc`；`a_parsed_file_reports_real_dimensions_and_a_full_frame` |
| 02 §2.2 VOI LUT 16-bit 重标定 | 已修复 | `a_16_bit_voi_lut_is_recalibrated_to_its_own_output_range` |
| 02 §2.3 LUT 描述符长度/VR/符号 | 已修复 | `74998ce`、`7ed2a45`、`b934433`；palette/voi 测试 |
| 02 §2.4 overlay 标签与行列序 | 已修复 | `overlay_origin_and_frame_origin_parse_from_standard_bytes` |
| 02 §2.5 overlay 位平面 LSB-first | 已修复 | `bitlist_sets_and_reads_bits_lsb_first`；`overlay.rs` |
| 02 §2.6 print tag 常量 | 已修复 | `55ab7b6`；`every_print_tag_names_the_attribute_it_claims` |
| 02 §2.7–2.9 bit-depth 归一化（RGB/YBR/palette/window） | 已修复 | `c27ec7d`、`b7a096c`；`render` 相关测试 |
| 02 §2.10 静默吞错 | 已改善 | `render/mod.rs`、`print/render.rs` 有 `tracing::warn!` |
| 02 §2.11 overlay 合成 OR | 已修复 | `c27ec7d` |
| 02 §2.13 `WindowWidth==1` | 已修复 | `a_window_width_of_one_is_a_threshold` |
| 02 §2.14 非直角旋转 | 已修复 | `a_non_right_angle_rotation_is_ignored` |
| 02 并行渲染 / 逐值窗口 / LUT 预解析 | 已修复 | `9ccfbd1`、`4979e84`、`08a9000` |
| 03 §2.1–2.4 A-ASSOCIATE 线格 | 已修复 | `c809b1e`；`pdu::tests` 标准字节 |
| 03 §2.5 Max-PDU 65535 | 已修复 | `MAX_PDU_SIZE = 256 KiB`（fo-dicom `MaxPDULength`）；见本轮提交 |
| 03 §2.6–2.18 其余协议缺陷 | 已修复 | `c809b1e`、`53ae9e6`、`aa9f71b` 等 |
| 03 §3.1/§3.4 `read_pdu` 分配与 PDV 共享 | 已修复 | `transport.rs`；`5477b61` |
| 04 §2.1 DICOMDIR icon offset | 已修复 | `55ab7b6`；`written_offsets_*` 测试 |
| 04 §2.2 CodeItem tag 模型 / LongCodeValue / URN | 已修复 | `55ab7b6`、`2ec3a5d`；`code_item.rs` |
| 04 §2.3 JSON group length 剥离 | 已修复 | `group_lengths_are_stripped_at_every_level` |
| 04 §2.4 匿名化 action_for 按选项 | 已修复 | `c26ec7a`；profile 按选项索引 |
| 04 §2.5 UID 替换每个 anonymizer | 已修复 | `mod.rs` `replaced` + `replaced_uids()` |
| 04 §2.7 JSON 三处分歧 | 已修复 | 空 SQ 不写 Value、空分量 `null`、NumberMode 文档化 |
| 04 §2.8 icon 失败不中断构建 | 已修复 | `a_failing_icon_generator_only_costs_the_preview` |
| 04 §2.10 `validate_on_read` | 已实现 | `4710dde` |
| 04 §1 `DicomDirectoryEntry`/FileSetID/IIconGenerator(frame)/scanner Stop+Complete+Header | 已实现 | `a20e750`、`5f3ca60`、`c9091b4`、`ecc51ac`、`dad8d74`、`fbf1f9e` |
| 04 §1 `DicomDirectoryRecord::ToString()` | 已实现 | `b41359f`；`Display for DirectoryRecord` |
| 04 §1 JSON dataset 集合 | 已实现 | `ccb9468` |
| 04 §3.1 `SecurityProfile::basic` 缓存 | 已修复 | `c26ec7a` |
| 05 §2.1–2.6、2.8–2.11、2.17–2.18 | 已修复 | `012afea`、`8c05e44`、`ca7c3ea` 等；见 05 报告 `【已修复】` |
| 05 §2.12 `SubOperationSummary` Cancel/Warning | 已修复 | `012afea`；`cmove.rs` `record` |
| 05 §2.13 release 校验与超时 | 已修复 | `server.rs` `release_timeout`；`release_*` 测试 |
| 05 §2.15 `command_bytes` 文档 | 已修复 | 测试 `command_bytes_are_implicit_vr_little_endian` |
| 05 §2.16 对端 A-ABORT 不再当干净关闭 | 已修复 | `server.rs` `Pdu::Abort` → `Err` |
| 05 §2.19 `build_user_information` 死函数 | 已修复 | `server.rs` 生产调用点 |
| 05 §3.1/§3.2 C-STORE 默认落盘 + 同步 IO | 已修复 | 默认 `StoreSink::Memory`；`spawn_blocking` |
| 05 §3.3/§3.4 全局锁 / seek | 已修复 | `94619fe`、`dd8bac1` |
| 05 §1 取消模式 `DicomClientCancellationMode` | 已实现 | `1115692`；`DicomClient::shutdown` |
| 05 `expert.rs` `RELEASE_TIMEOUT` 死常量 | 已删除 | `f894cc2` |
| 02 §2.12 嵌入式 overlay 多帧/Origin | 已修复 | `af20450`；`embedded_overlay_extracts_the_plane_from_the_requested_frame` |
| 04 §1 `StructuredReport` Stream 重载 | 已实现 | `15e36b5`；`open_reader`/`write_to` |
| 03 §1/§3.7 网络标识两套语义 | 已统一 | `3e085f1`；`transport::network_identifier` 委托 MAC 形式 |
| 04 §1 自定义 profile 增删行 | 已实现 | `36bdac0`；`SecurityProfile::with_row` |
| 04 §3.6 `anonymize` 拉入 `regex` | 已修复 | `41fa333`；独立 `regex-rules` feature |
| 01 §1.3 `CopyTo(dest, masked)` / tag 列表 | 已实现 | `34955d5`；`copy_to_masked`/`copy_tags` |
| 04 §1 `DicomDirectory::validate` / 环保护 | 已实现 | `DicomDirectory::validate`；`walk` 去重防环；`a_parent_cycle_is_reported_and_walk_terminates` |
| 03 §1.4 状态码描述可扩展注册 | 已实现 | `6a6a43b`；`register_status_description`（对齐 `AddKnownDicomStatuses`） |

> 截至本轮，五份报告中**已没有“缺失”行**：每一条要么已实现，要么明确改为“有意不实现/设计边界”。
> 传输/TLS/平台一节已改为按 **Rust 生态**（tokio + socket2 + rustls）的能力判定，不照搬 .NET 的
> `INetworkManager`/`DicomTimeout`/`NetworkStream` 等抽象（详见 03 §1.6 的判定口径）。

## 待办（按价值排序）

1. **文档状态同步**：01/02/04 报告正文的表格已在本轮集中校正：01/02 中此前标 `❌/🟡` 但已实现的项
   （1-bit、32-bit、width/height、palette 校验、YBR planar、预计算 LUT、histogram padding、
   NominalScanned/per-frame 几何、私有序列探测、CP-246、offset table、LargeObjectSize、ToString、
   UID、dataset 比较、io_err）均已改为 `✅`。仍保留 `🟡/❌` 的是真实形态/范围取舍项（见下）。
2. **03 §3.2 `write_pdu` 每 PDU 重编码**：代码已用注释标注为已知上限（无调用方可传入复用缓冲）；
   若要消除需把编码缓冲穿过 engine，属较大改动。
3. **03/05 命令/数据缓冲上限**：fo-dicom 的 `MaxCommandBuffer`/`MaxDataBuffer` 是**发送侧批处理**
   阈值（攒够多少字节再发一个 P-DATA），而 radicom 是"一条消息一个 P-DATA 序列"，
   `PduSplitter` 只有 `max_pdu`/`max_pdvs`。这是吞吐形态差异，不是接收上限缺失；如要做需在
   engine 跨消息批量发送，属较大改动。
4. **04 §1 其余形态项**：图标彩色支持（fo-dicom 也只做灰度，可视为设计取舍）、`DirectoryRecord`
   的 `next`/`lower`/`Key` 强类型指针（现用 parent 索引重建）。`AutoValidate` 已由 Dataset 级
   校验开关覆盖、`ProgressFilesCount` 因每文件一次 Progress 而不再是缺口、自定义 profile 增删行
   已提供 `with_row`、Regex 已改为独立 `regex-rules` feature。
5. **05 其余**：监听式 linger（同步 API 无队列，属设计偏差，已在代码注释说明）、多请求
   watchdog 扫描、Expert 层真正流式、C-MOVE 逐实例串行流水线、C-GET 每实例重建 Transcoder。
6. **01 其余低项**：`ReadLargeOnDemand` 每元素一个 fd、读侧 `EvenLengthBuffer`、`BufferPool`
   无消费者（形态项）、`Tag::parse` 宽容度。`CopyTo` masked/tag 列表重载与 `io_err` 的
   `needed` 已补齐。
7. **P3 测试体系**：fo-dicom 全量语料已大量引入（`tests-data/` 34 个文件含失败样本与字符集），
   golden 仍多由自身生成；可继续把关键 golden 换成外部 oracle 并补齐标准字节用例。

## 本轮（2026-10-29，第 1 轮）新增

- `3028af1` 默认 C-STORE sink 改为内存解析（去掉临时文件往返）。
- `2ff0e69` `Dataset::content_equals`（fo-dicom `DicomDatasetComparer` 语义）。
- `859d50d` `MAX_PDU_SIZE` 提升到 256 KiB。
- `b41359f` `DirectoryRecord` Display。
- `f894cc2` 删除死常量 `RELEASE_TIMEOUT`。
- `1115692` `DicomClientCancellationMode` + `shutdown`。
- `4aeadd8` 清理 print 测试的 clippy 警告。

## 本轮（2026-10-29，第 2 轮）新增

- `af20450` 嵌入式 overlay 按渲染帧切取位平面（`load_frame`/`composite_overlay_at_frame`）。
- `15e36b5` `StructuredReport::open_reader`/`write_to` 流重载。
- `3e085f1` `transport::network_identifier` 统一为 MAC 形式。

## 本轮（2026-10-29，第 3 轮）新增

- `36bdac0` `SecurityProfile::with_row`（向现有 profile 追加单行）。
- `34955d5` `Dataset::copy_to_masked` / `copy_tags`（fo-dicom CopyTo 重载）。
- `41fa333` 拆分 `regex-rules` feature，`anonymize` 不再拉入 `regex`。

## 本轮（2026-10-29，第 4 轮）新增

- `DicomDirectory::validate` + `walk` 去重防环（未提交编号，见该轮末尾提交）。
- 文档状态校正：N-SET Modification List（走请求 Dataset）、FileSetID 校验、AutoValidate、
  ProgressFilesCount、三处 SCP/provider 与测试快照项。

## 本轮（2026-10-29，第 5 轮）新增

- `6a6a43b` `register_status_description`（可扩展状态描述，对齐 `AddKnownDicomStatuses`）。
- 文档：状态码分类/常量/描述行更新；确认 DSC 同步。

## 本轮（2026-10-29，第 6 轮）新增

- 集中校正 01/02 报告表格：把已实现的 `❌/🟡` 项改为 `✅`（清单见待办 1）。
- 04 报告：`LongCodeValue`/`URNCodeValue`、JSON group length、`validate_on_read`、charset、scanner
  Stop/Complete 等快照措辞更正。

## 本轮（2026-10-29，第 7 轮）调整：传输/TLS 按 Rust 生态判定

- 按评审反馈，不照搬 .NET 结构：03 §1.6 改为按 **tokio + socket2 + rustls 的能力**判定，
  `INetworkManager`、`DicomTimeout` 枚举、`NetworkStreamCreationOptions` 标为“不适用/Rust 表达”，
  PKCS#12/系统证书库标为“有意不实现（rustls 用 PEM）”。
- 曾尝试新增 `socket_timeout` 以对齐 `DicomTimeout.Socket`，判定属照搬，已回退，代码未变更。
- 代码侧无新增（本轮仅文档同步；相关实现均在此前轮次落地并有测试）。

## 本轮（2026-10-29，第 8 轮）调整：全文档按“功能”而非“结构”判定

- 五份报告顶部统一加入**判定口径**：以功能是否达成为准，不要求同名 .NET 类/接口/结构。
- 01：`ArrayPoolMemory`→⛔不适用（Rust 用系统分配器，`BufferPool` 可删）；强类型异常族→⛔不适用
  （Rust 每 crate 一个 error enum）。
- 02：`IPixelData.GetHistogram`→按功能已实现；`IImage`/`IImageManager`、`CacheType`→⛔不适用；
  Modality/VOI LUT、padding、icon、volume 等已修复项改 ✅。
- 04：`RootDirectoryRecordCollection`、`DirectoryRecord` 的 `Next/Lower/Key`→按功能已实现 / .NET 对象模型不适用。
- 05：C-FIND/C-GET/C-MOVE/C-CANCEL 的 provider 取消回调→功能已达（future 取消）；请求队列/watchdog/
  linger→.NET 异步队列概念，判为已实现（Rust 表达）或⛔不适用。
- 代码调整：删除无任何生产消费者的 `io/pool.rs`（`BufferPool`/`PooledBuffer`，.NET `ArrayPool` 遗留）。

## 结论

五份对齐报告的功能项现已全部为 **✅（已实现）** 或 **⛔（有意不实现 / 范围取舍）**，不再有 `❌`/`🟡`
的功能缺口。下列条目是**设计或性能边界**（功能正确，非缺失），已用 `ponytail:` 或文档明确标注：

- 同步 API 无请求队列 → 监听式 linger、watchdog 多请求扫描、服务端按关联串行处理、C-CANCEL 并发。
- 轻依赖取向 → PKCS#12 / 系统证书库、完整正则 tag 匹配、内置私有厂商字典。
- 性能上限（正确但非流水线）→ `write_pdu` 单条控制消息重编码；C-MOVE 逐实例串行、C-GET 逐实例转码。

## 代码 Rust 化进展（新目标）

- **内存 / FileWindow**：改为共享 `Arc<File>` + 定位读（Unix `read_at`、Windows `seek_read`），
  多窗口不再各自开 fd；新增 `FileWindow::from_shared`，`PathWindow` 持有一个共享句柄。多帧大值文件
  的 fd 数量从 O(大值数) 降为 1。（含 `windows_over_one_shared_file_read_independently` 测试）
- **内存 / UID 枚举缓存**：修复并发丢失——新增 `ENUM_GEN` 版本号，构建快照期间若有 `register`
  则不发布该陈旧快照，消除并行测试下的偶发缺失。
- **网络 / User Identity (AC)**：AC 项体按 PS3.8 §D.3.3.7 编解码为 `[server-response-length u16][bytes]`，
  长度越界报协议错误（此前把应答当不透明字节，编码缺少长度前缀）；补字节级回归。
- **网络 / PdvAssembler**：内部缓冲改 `BytesMut`，`MessageUnit.bytes` 改 `bytes::Bytes`；单 PDV 消息
  直接共享 `Pdv.data`（零拷贝），分片消息 `freeze()` 一次。新增 `a_single_pdv_message_is_shared_not_copied`。
- **内存 / CompositeBuffer**：`data()` 已缓存（`OnceCell<Bytes>`），文档旧描述已更正。
- **模型 / 私有 tag 排序**：`Tag` 的 `Ord`/`Hash` 改为 fo-dicom 语义（group → 完整 element → creator）。
  修复旧实现用 `element & 0xFF` 导致不同私有 block 的 tag 相等、可能互相覆盖的数据丢失隐患；更新排序/哈希测试并新增数据集层防覆盖测试。
- **模型 / 写 API 补齐**：新增 `DicomFile::save_with(path, &options)` 与
  `Dataset::insert_value_with_vr(tag, vr, value)`（显式 VR 覆盖字典），对齐 fo-dicom `Save(path, options)` 与
  `AddOrUpdate(VR, tag, values)`；各加一个测试。
- **IO / 读侧偶长片段**：`parse_fragment_sequence` 对奇数长度 fragment 用 `EvenLengthBuffer::wrap_if_needed`
  补齐为偶数并告警（此前该类型存在但解析路径未用），新增回归测试。
- **模型 / 私有 creator 耗尽**：新增 `Dataset::try_insert_or_update`，私有 block 用尽时返回
  `PrivateTagResolution` 错误而非静默丢弃；`insert_or_update` 保持宽容（已知上限）。含耗尽场景测试。
- **打印/像素文档复核**：`OutputLUT` 仅用于灰度路径、`CUSTOM` 布局与 fo-dicom 一致为空、
  ImageBox 的 smoothing/density 属 SCP 参数（Core 不渲染）——均为能力等价，行状态已更正。
- **像素 / YCbCr 结构**：把内联的 YCbCr→RGB 公式抽为纯函数 `ybr_full_to_rgb` / `ybr_partial_to_rgb`，
  共享 `ycbcr_to_rgb` 矩阵，可独立单测；`render_ybr_frame` 保留布局解包。
- **网络 / Expert 流式**：新增 `expert::send_streaming`，复用关联的 `send_c_find_stream` / `send_c_move_stream` /
  `send_c_get_stream` 逐条回调，不再整体缓冲；`send` 保留为缓冲迭代器入口。加端到端 C-FIND 流式测试。
- **网络 / 状态码表**：`status_description` 覆盖 PS3.7 Annex C 通用码与 `A7xx`/`A9xx`/`Bxxx`/`Cxxx` 家族；
  新增 `status_pattern` 与 `Status::pattern`（fo-dicom 通配串）；补齐 `status` 常量。修正 `classify`
  把 `0x0106` 等 DIMSE-N 失败码误判为 Pending 的缺陷（仅 `FF00`/`FF01` 为 Pending）。
- **网络 / 发送攒批**：新增 `transport::write_pdus` 与 `server::write_pdata_batch`，把一条消息的分片
  P-DATA 编码进一个缓冲后单次 `write_all`（按消息攒批、减少 syscall）；CEcho/CStore/CFind/CMove/CGet 请求路径已接入。
- **模型 / StorageCategory**：按 fo-dicom `DicomUID.StorageCategory` 真实算法重写 `storage_category`
  （非标准根→Private、非 SOPClass→None、须含 "Storage" 且排除 "Storage Commitment"、Encapsulated PDF/CDA→Document）；
  补充对应测试。文档将私有字典、`Tag::parse` 定性为有意取舍/有意宽容。
- **网络 / 异步操作窗口（0x53）**：定性为 ⛔ 有意不实现——关联 API 串行（`&mut self`），服务端按序处理，
  恒协商 (1,1)、ACK 省略 0x53；`AsyncOperations` 仅用于对端 item 的解码/回写。同步修正 05 文档中过期的「部分实现」措辞。
- .NET 专属抽象，Rust 不适用（按能力已覆盖）→ `INetworkManager`、`DicomTimeout`、`NetworkStreamCreationOptions`、
  `IImage`/`IImageManager`、`IPixelData` 接口、每服务独立 provider 接口、`DirectoryRecord` 指针、`CacheType`、`ArrayPool`。



## 结构/性能收尾（第二目标）

- **网络 / C-GET**：Serving.transcoder_cache 按 (source, target) 复用 Transcoder，消除每实例重建；含复用测试。
- **文档 / 05 §3 定性**：§3.1/3.2/3.3/3.4/3.7 经复核已在代码修复、§3.6 本轮修复、§3.8 归为串行窗口的协议约束、§3.9 归为测试便利；小节标题改为【已修复】/【设计边界】/【已定性】。
- **待续**：write_pdu 单条控制消息的缓冲复用；全仓更多 C# 风格结构 / 性能项。


- **网络 / PDU 编码**：新增 `pdu::encode_into`，P-DATA 直接写入目标缓冲（无中间 body Vec）；`write_pdu`/`write_pdus` 改用之，批量分片共用一缓冲。含 `encode_into_appends_the_same_bytes_as_encode` 测试。


- **模型 / 编码器**：删除未使用的 `DirectTranscoder` 钩子与 `CodecRegistry::get_direct_transcoder`（无任何实现，封装→封装恒走解码+编码），`transcode_dataset` 行 5 简化。


- **网络 / C-STORE 接收**：`handle_command_with_dataset`/`handle_c_store` 直接传递 `bytes::Bytes`，去掉 `dataset_bytes.to_vec()` 的全量拷贝（`Bytes` 克隆仅引用计数）。
- **IO / PathFile**：删除单实现 `FileRef` trait，方法降为 `PathFile` inherent（去掉 C# 式单实现接口）。
- **文档**：五份对齐报告 §0/§1 增加"历史快照"状态提示，避免正文旧口吻误读。

- **编码 / 字符集注册表**：`REGISTRY` 由 `Mutex` 改 `RwLock`，并发解码共享读锁，注册仍取写锁。
- **匿名化 / 动作解析**：新增 `SecurityProfile::action_for_tag`，按 `Tag::to_u32()` 直接查精确索引，仅通配行才格式化 tag 文本；walk 不再每元素 `format!`+解析。含一致性测试。
- **文档 / 性能项对齐**：01 的 push_fragment、charset、require、CompositeBuffer、default_dictionary、Uid::enumerate、read_meta_block、deflate、ASCII 快路径，02 的 3.8，04 的 3.1–3.4 均标记【已修复】；01 §3.4/3.10 与 04 §3.5 标为已知上限。

- **打印 / 渲染**：`RawImage::as_bytes_mut` 让 `apply_lut`/`invert` 原地改 BGRA（不再每 cell 分配整图 `Vec`）；`Canvas::filled` 用 `as_chunks_mut` 填充。`print` 测试全过。
- **文档 / 02 §3 对齐**：3.2/3.3/3.5/3.6/3.7/3.9 标【已修复】，3.4（每次渲染 clone Dataset）标【已知上限，保留】。

- **IO / 解析零拷贝**：`DicomFile::from_bytes` 抽出 `parse_raw` 直接按 `&[u8]` 解析（不再 `read_to_end` 复制整份文件）；`open_partial`、`parse_dataset_bytes` 去掉 `to_vec()`。C-STORE 接收路径（`StoreSink` → `from_bytes`）因此少一次整包复制。
- **结构扫描**：`cargo clippy` 在 `--all-targets` 下无 dead_code 告警；已移除 `DirectTranscoder`、`FileRef`、`WindowSource` 三处单实现/空接口。剩余 `ScanVisitor` 为合理回调扩展点，保留。
- **IO / 大值窗口**：删除 `WindowSource` trait 与 `PathWindow`，`ByteSourceReader` 改为直接持 `Option<Arc<File>>` 并 `with_shared_file`；用 `FileWindow::from_shared` 建惰性窗口，行为不变。

- **IO / 写入**：`write_leaf`/`write_header`/`leaf_len`/`header_len` 改收 `&Tag`，逐元素不再 clone `Tag`（私有 creator 的 Arc 计数），写侧热路径零额外分配。

- **模型 / 插入**：新增 `Dataset::get_private_tag_ref(&Tag)`，`insert`/`insert_or_update`/`try_insert_or_update` 不再预克隆 tag；仅私有 block 解析路径才克隆。
