# fo-dicom 对齐审计（模块级，代码走查版）

本文件是对 `radicom` 与 fo-dicom 功能面对齐情况的**逐模块代码走查结论**，回答「与 fo-dicom 对齐下哪些完成、哪些未完成」。

## 方法与证据标准

- 不采信文档自报状态：逐模块读代码（符号/实现/测试）+ 运行
  `cargo test --workspace --all-features` 与 `cargo clippy --workspace --all-features --all-targets`。
- 判定口径：
  - **DONE**：有真实实现、有行为测试、端到端数据流完整（非仅字段/仅标签）。
  - **剩余-范围**：实现依赖本仓库之外的独立工程（如 `radicom-codecs`）。
  - **剩余-环境**：需本机不具备的外部依赖/对端。
  - **剩余-平台**：需本机之外平台 FFI（如 macOS）。
  - **不需要**：fo-dicom 用 .NET 原生能力、Rust 无对应内置且强行引入会带来额外依赖；按用户意见不强加。

## 结论

**P0/P1 及 P2 核心功能全部 DONE。** 未完成项仅有：具体编解码器实现（属外部 `radicom-codecs` 工程）、macOS MAC（平台）、reference-peer 互通（环境），以及若干"单实现/无消费者"的形态类抽象（按 Ponytail 不造）。监控（OBS）已用 `tracing` 字段约定实现，**不引入 `opentelemetry` 依赖**——符合"不额外引监控"的要求。

## 已完成（DONE，含代码证据）

| 模块组 | 覆盖特性 | 证据（文件/符号） |
|---|---|---|
| 标识 (TAG/VR/UID/TS/DICT/VM) | TAG-01..05、VR-01..02、UID-01..05、TS-01..03、DICT-01..03、VM-01 | `model/{tag,masked_tag,tag_index,vr,uid,transfer_syntax,dictionary,vm}.rs`；`network/print_sop.rs`(UID-04)；`Dictionary::load_xml`+默认替换(DICT-03)；`TransferSyntax::register`(TS-02) |
| 数据集/元素 (DS/ELEM/MATCH/XFORM) | DS-01..10、ELEM-01..05、MATCH-01、XFORM-01 | `model/dataset.rs`（`get_*`/`try_get_*`/`insert`/`set_validation`/`functional_groups`/`DatasetWalker`/`copy_to`/`get_datetime`/`enumerate_group`/`enumerate_masked`）、`model/element.rs`（DS 逗号容忍 "1,5"→1.5）、`rules/{match_rule,transform}.rs` |
| 字符集/校验 (ENC/VAL) | ENC-01..04、VAL-01..03 | `encoding/charset.rs`（`register_charset`+`charset_for_encoding`）、`validation/mod.rs`（全局开关+`ValidationScopeGuard`） |
| 文件 IO (FILE/RD/WR/BUF/IO/MEM) | FILE-01..06、RD-01..05、WR-01..03、BUF-01..02、IO-01..03、MEM-01 | `io/{file,reader,writer,dataset_builder,byte_source,file_ref,pool}.rs`；`LargeValuePolicy`(FILE-02)、`has_valid_header`(FILE-06)、`item_len/header_len`(WR-02)、deflate(RD-04) |
| 像素/编解码接口 (PIX/CODEC/TR) | PIX-01..05、CODEC-01..03,05、TR-01..03 | `pixel/{mod,codec,overlay,photometric,transcoder}.rs`；`uncompressed_frame_size`(PIX-03)、palette LUT(PIX-04)、`CodecRegistry`/`CodecParams`/JPEG 探测(CODEC)、`Transcoder`+`Dataset/DicomFile::clone_with_syntax`(TR-03) |
| 渲染 (REND) | REND-01..11 | `render/{mod,bit_depth,color_table,geometry,icon,interpolation,math,pixel_converter,reconstruction,spatial}.rs`；全 PI 分派、模态/VOI/palette LUT、GrayscaleRenderOptions、BitDepth、PixelDataConverter、spatial(scale/rot/flip/pan)、MPR(reconstruction)、math(Matrix/Histogram/BitList/MovingAverage/Rect/Point2)、**手写零依赖 PNG/BMP**(`to_png`/`to_png_rgb`/`to_bmp`/`save`) |
| 网络 (NET/SRV/CLI/HOST/DIMSE/OBS) | NET-01..08、SRV-01..08、CLI-01..02、HOST-01、DIMSE-01、OBS-01/02 | `radicom-network/src/*.rs`；C-ECHO/STORE/FIND/MOVE/GET、N-服务、C-CANCEL(`client.cancel`/`send_c_cancel`)、PDV 组包(`PduSplitter`/`split_message`)、扩展协商(`extended.rs`)、TLS(`tls.rs`)、托管(`hosting.rs`)、`observability.rs`(OBS-01 + `otel` feature 下 `TracingMetrics`，无 opentelemetry 依赖) |
| 序列化 (JSON/XML) | JSON-01..02、XML-01 | `radicom-json/{lib,xml}.rs`（NaN/Inf 字符串、BulkDataURI 双向、PN 三组、XML NativeDicomModel 结构） |
| 媒体 (MEDIA) | MEDIA-01..03 | `media/{mod,icon,scanner}.rs`（DICOMDIR 读写、图标生成、异步扫描） |
| 结构化报告 (SR) | SR-01..02 | `sr/{content_item,code_item,structured_report}.rs`（15 值类型含 SCOORD3D、7 关系、MeasuredValue/ReferencedSop、构建+写回+遍历+open/save） |
| 打印 (PRINT) | PRINT-01..02 | `print/{models,layout,render}.rs`（`initialize_image_boxes`、`render_to_image`） |
| 匿名化 (ANON) | ANON-01 | `anonymizer/{mod,profile}.rs`（PS3.15 全表、11 选项、序列递归、UID 一致替换） |
| 基础设施 (INF) | INF-01..07 | `error.rs`、`logging.rs`、`dump/mod.rs`、`bin/radicom-dump.rs`、`xtask/src/main.rs`(代码生成) |

## 未完成（明列，不谎称完成）

| 项 | 类别 | 原因 / 现状 |
|---|---|---|
| JPEG/JPEG-LS/JPEG2000/HTJ2K/RLE 具体编解码实现 | 剩余-范围 | 按项目架构属于**外部 `radicom-codecs` 工程**（本仓库仅定义 `Codec` trait、`CodecParams`、`CodecRegistry` 注入点与 `Transcoder` 流程；离线且该 crate 不在工作区） |
| UID-05 macOS MAC 读取 | 剩余-平台 | 需 CoreWLAN/IOKit FFI（Windows `GetAdaptersAddresses` 与 Linux 已实现并有测试；macOS 非本机平台） |
| reference-peer（DCMTK/fo-dicom）网络互通回归 | 剩余-环境 | 需外部真实对端；本地为 loopback 自测（245 网络测试通过） |
| `image` crate 图像后端（PNG/JPEG 的 image-rs 版本） | 剩余-可选 | **未引入该依赖**；但 PNG/BMP 导出已由手写零依赖编码器覆盖（`to_png`/`to_bmp`），导出能力不缺 |
| `IPixelData`/`Image` trait、`InvertLut`/`PaddingLut`/`CompositeLut`/`PrecalculatedLut`、`CacheType` 等 | 剩余-形态 | fo-dicom 内部架构用的单实现/无外部消费者抽象；Rust 侧已有等价具体实现（`RawImage`/`BitDepth`/`ColorTable` 等），按 Ponytail 不造空壳 trait |
| OBS 额外监控接入 | 不需要 | 监控已由 OBS-01（`MetricsCollector`）+ OBS-02（`otel` feature 下 `TracingMetrics`，经 `tracing` 字段约定输出，**不引 `opentelemetry` 依赖**）完成；符合"Rust 无内置则不强加额外监控" |

## 复现

```pwsh
cargo test --workspace --all-features
cargo clippy --workspace --all-features --all-targets
```

## 当前测试基线

17 个测试套件全绿：core **497**、network **245**、json **34**、corpus **5** + 集成，0 失败；clippy 无警告。
