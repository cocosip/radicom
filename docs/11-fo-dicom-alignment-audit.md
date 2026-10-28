# fo-dicom 对齐审计（模块级）

本文件汇总针对 `radicom` 与 fo-dicom 功能面对齐情况的分模块核查结论，用于回答
目标中反复出现的「与 fo-dicom 对齐下是否存在功能缺失」。

## 方法与证据标准

- **不采信文档自报状态**：逐模块读代码 + 读测试 + 运行 `cargo test --workspace --all-features`
  与 `cargo clippy --workspace --all-features --all-targets`，以行为可见的证据判定。
- 涉及标准语义时，以 DICOM 标准（PS3.3 / PS3.5 / PS3.15 等）原文为准（必要时 web 取权威页面）。
- 判定口径：
  - **DONE**：有真实实现、有行为测试、端到端数据流完整（非仅字段/仅标签）。
  - **剩余-环境**：实现需要本机不具备的外部依赖（如 `image` crate、外部对端 DCMTK/fo-dicom）。
  - **剩余-平台**：需要本机之外的平台 FFI（如 macOS）。
  - **剩余-形态**：fo-dicom 内部架构用、单实现、无外部消费者的类型（Ponytail 原则不造）。

## 结论摘要

**用户可见的功能面完整，与 fo-dicom 对齐良好。** 未发现仍存在的用户可见功能缺口；
剩余项仅为「环境/平台受限」与「内部实现形态」两类，均不影响可用功能。

## 模块级结论

| 模块 | 关键能力 | 状态 | 证据 |
|---|---|---|---|
| 编码/字符集 (ENC) | ISO_IR 6/13/100/101/109/110/126/127/138/144/148/149/166/192、GBK、GB18030；全部 ISO 2022 变体（JIS X 0208/0212、Korean ESC`$`)C`、GBK ESC`-C`）；解码+编码+回退 | DONE | `encoding/charset.rs`，往返/回退测试 |
| 文件 IO (IO/RD/WR) | 显式/隐式 VR、大小端、Deflated（raw DEFLATE 写、zlib 回退读、往返+稳定性）、值奇偶补齐（PS3.5 §6.2） | DONE | `io/*.rs`，`deflated_*`、`odd_length_string_is_padded` |
| 像素/编解码 (PIX/TR) | `uncompressed_frame_size`（1-bit/422/420/奇数宽）、`Transcoder`（真实重编码、内嵌 overlay 提取、lossy 标记、meta 组同步）、`DirectTranscoder` 注册 | DONE | `pixel/{mod,transcoder,codec}.rs` |
| 渲染 (REND) | 全 PI 分派、模态/VOI(LINEAR/EXACT/SIGMOID)/输出/palette LUT、GrayscaleRenderOptions、ColorTable(+load/save)、BitDepth、ColorSpace、PixelDataConverter、overlays、spatial(scale/rot/flip/pan)、BMP/PNG、DicomImage 门面 | DONE | `render/*.rs`，含本轮新增 RGB 归一化、MONOCHROME1、多帧逐帧窗口测试 |
| 序列化-媒体 (MEDIA) | DICOMDIR 构建/读写（偏移符合 PS3.3 F.3.2.2 绝对文件字节偏移）、图标生成（≤128、8-bit、降采样）、媒体存储 | DONE | `media/*.rs` |
| 网络 (NET/DIMSE) | C-ECHO/STORE/FIND/MOVE/GET、N-服务、PDV 组包（bit0/bit1）、分片、状态码分类（掩码类）、C-CANCEL | DONE | `radicom-network` 245 测试、loopback |
| 安全/匿名化 (ANON) | PS3.15 全表（~655 行 CSV）、11 选项、序列递归、与 fo-dicom 测试交叉验证 | DONE | `anonymizer/*.rs` |
| 结构化报告 (SR) | 15 值类型、7 关系、构建+写回 `(0040,A730)`、遍历 | DONE | `sr/*.rs` |
| 数据模型 (model) | 标签/VR/PNN/DA-TM-DT（分数秒≤6位、`:`/`.`、双向）、`Uid` 校验 | DONE | `model/*.rs` |
| 打印 (PRINT) | render_to_image（Trim/Polarity/等比居中）、打印 SOP | DONE | `print/*.rs` |
| JSON/XML | DICOM JSON（NaN/Inf 保留字符串、BulkDataURI 双向）、XML 转义 | DONE | `radicom-json` 33 测试 |

## 本轮核查期间修复的真实缺陷

| 缺陷 | 修复要点 |
|---|---|
| VOILUTFunction (0028,1056) 从未读取，SIGMOID 不可达 | 读取并按 LINEAR/EXACT/SIGMOID 分派 |
| Palette 忽略 FirstMappedPixelValue | 输入值 `v - first`（含负值/超界 clamp） |
| SpatialTransform.pan 声明但从未应用 | `apply_to` 加入整数像素平移（黑填充） |
| ACR-NEMA 老图像无 PI 时未推断 | `infer_from_dataset`：≤1 sample→PALETTE/MONOCHROME2，多 sample→RGB |
| DICOMDIR 图标 NumberOfFrames VR 写成 CS | 改为 IS（PS3.6） |
| RGB 渲染忽略 BitsStored/HighBit，`v>>8` 使低对齐 12-bit 变黑 | 经 `BitDepth` 归一化后再缩放 |
| DicomImage 分派误用 samples-per-pixel（palette 1-sample 走灰度） | 改为按 PI 分派 |

## 本轮核查期间补齐的真实功能

`RawImage::scaled`、`DicomImage` 门面、`ColorTable`（含 8-bit 映射与 `.lut` load/save）、
`PixelDataConverter`、`BitDepth`、`ColorSpace/ColorSpaceType`、BMP 导出、`RawImage::save(path)`、
**UID-05 的 64 位 Windows MAC 读取**（`GetAdaptersAddresses` raw FFI，本机真实网卡测试通过）。

## 明确剩余（不谎称完成）

| 项 | 类别 | 原因 |
|---|---|---|
| `image` crate 后端 / JPEG 导出 | 环境 | 离线环境无该 crate |
| reference-peer（DCMTK/fo-dicom）网络互通 | 环境 | 需外部对端 |
| UID-05 macOS MAC | 平台 | 需 CoreWLAN/IOKit FFI，非本机平台 |
| `IPixelData`/`Image` trait、`InvertLut`/`PaddingLut`/`CompositeLut`/`PrecalculatedLUT`、`CacheType` | 形态 | 单实现/无消费者；按 Ponytail 不造 |

## 复现

```pwsh
cargo test --workspace --all-features
cargo clippy --workspace --all-features --all-targets
```
