# 02 — pixel / render / print 审查报告

> 状态提示：本文件是**走查当时的快照**，其中多数“高”缺陷此后已修复。逐条当前处置与证据见
> [`PROGRESS.md`](PROGRESS.md) 与源码/测试；本文正文保留原始结论用于追溯。


审查范围：`crates/radicom/src/pixel/`（`mod.rs`、`codec.rs`、`overlay.rs`、`photometric.rs`、`transcoder.rs`）、`crates/radicom/src/render/`（`mod.rs`、`bit_depth.rs`、`color_table.rs`、`geometry.rs`、`icon.rs`、`interpolation.rs`、`math.rs`、`pixel_converter.rs`、`reconstruction.rs`、`spatial.rs`）、`crates/radicom/src/print/`（`mod.rs`、`models.rs`、`layout.rs`、`render.rs`）。

参照实现：`D:\Code\dotnet-source\fo-dicom\FO-DICOM.Core\Imaging\`、`FO-DICOM.Core\Printing\`、`Platform\FO-DICOM.Imaging.Desktop\`。
按要求排除：具体图像编解码实现（JPEG / JPEG-LS / J2K / HTJ2K / RLE 的编解码）。但 `pixel/codec.rs` 的注册表接口与 `pixel/transcoder.rs` 的决策表在审查范围内。

方法说明：结论全部来自实际阅读代码 + 实际运行验证。凡是涉及"行为是否正确"的条目，都用最小 `Dataset`（含真实解析文件）跑探针确认，探针程序位于系统临时目录，未进仓库。关键结论使用 fo-dicom 自带 benchmark 数据（`Tests\FO-DICOM.Benchmark\Data\ct.dcm`、`mr.dcm`）作为真实解析文件样本。仓库自带 `docs/*.md` 的完成度声明未被采信。

工作区基线由 Lead 预先验证（`cargo build/clippy/test` 全绿）；本报告不重复验证，且**未修改任何源码或测试**。

---

## 0. 结论摘要

> **状态提示**：本报告是首次对齐走查的**历史快照**，正文以"当时未修复"的口吻记录问题；所列缺陷/缺失大多已在后续提交修复，当前能力与状态请以 §1 表格（✅/⛔）及 `PROGRESS.md` 为准。

本模块最严重的问题不是"缺几个 PI"，而是**这条渲染链路在真实文件上根本不通**，而且**测试写法系统性地掩盖了它**：

1. **【致命】`PixelData::width()` / `height()` 用 `u32` 去读 VR=US 的 `(0028,0011)`/`(0028,0010)`，对任何"解析出来的文件"恒返回 0**（`pixel/mod.rs:126-132`）。根因在 `model/element.rs:192-206` 的 `read_le_unit`：`unit = e.vr().unit_size().max(T::SIZE)` 对 US(2) → u32(4) 取 `max = 4`，只为 2 字节的值索要 4 字节，返回 `IndexOutOfRange{index:0,count:0}`；`Dataset::get_or`（`model/dataset.rs:132-134`）把这个错误直接吞成默认值 `0`。**后果分两级**：
   - `render_frame()` 在所有真实文件上报 `frame has zero width or height`（实测 `ct.dcm` 512×512、`mr.dcm` 256×256 全部失败；fo-dicom 同样文件正常渲染 512×512 / 256×256）。
   - 更糟的是**静默数据丢失**：`uncompressed_frame_size()`（`pixel/mod.rs:181-205`）也返回 `Ok(0)`，于是 `native_frame()` 的越界检查 `if offset + frame_size > buffer.size()`（`pixel/mod.rs:232`）对 `0 + 0 > 524288` 判定为假，最终返回一个 **0 字节的 frame 且 `Ok`**。实测 `ct.dcm` 有 524288 字节像素数据，`get_frame(0)` 返回 `Ok(0)`；`mr.dcm` 同理。调用方写文件、算哈希、或转给对端，拿到的是一张空图，没有任何诊断。
   - 连带失效：`render/icon.rs`（图标渲染）、`render/reconstruction.rs:337`（MPR 槽位 = 0）、`pixel/transcoder.rs`（逐帧尺寸为 0）。
2. **【致命掩盖】`render/` 下 117 个测试（仅 `render/mod.rs` 就 50 个）全绿，是因为它们全部手搓 `Dataset`，从没让真实文件走过这条 `u32` 字节路径。** 实测对照：解析出的元素是 `vr=Us value=Bytes([0x00,0x02])`（`u32` 读失败），而测试里 `insert_value(ROWS, 4u32)` 存的是 `vr=Is value=Str("4")`（`u32` 走字符串解析成功）。也就是说测试与生产走的是两个不同的分支，前者永远绿。详见 §4.1。
3. **【高】16-bit VOI LUT 的输出缩放写错**：`render/mod.rs:99-105` 用 `v >> 8`，而 PS3.3 C.11.2.1.1 规定 LUT 输出范围是 `0..2^n−1`，fo-dicom 是把 `MinimumOutputValue/MaximumOutputValue` 接到 `VOILinearLUT` 上再经 `OutputLUT` 映射到 0..255（`GenericGrayscalePipeline.cs:146-147,161`）。实测：16-bit LUT 数据 `[0,1000]`，值 1000 应渲染成 255，radicom 渲染成 3。仓库里 `render/mod.rs:1521-1527` 的测试**把这个错误写成了期望值**。
4. **【高】overlay 的 `ImageFrameOrigin` 读错标签**：`pixel/overlay.rs:67` 用 `(60xx,0052)`/`(60xx,0053)`，而 PS3.3/字典里 `ImageFrameOrigin` 是 `(60xx,0051)`（US VM1），`(60xx,0052)` 是已退休的 `OverlayPlaneOrigin`、`(60xx,0053)` 在 overlay 组根本不存在。同时 `(60xx,0050) OverlayOrigin` 是 **row\column** 顺序（PS3.3 C.9.2），`overlay.rs:66` 却直接取 `[0]→x, [1]→y`。实测：`ImageFrameOrigin=3` 的 2 帧 overlay，被画到了第 0、1 帧（应为第 2、3 帧）；`OverlayOrigin=2\3` 的 overlay 被画在 (x=2,y=1) 而非 (x=1,y=2)。
5. **【高】print 模块两个 tag 常量取值错误**，导致 film box 的整个 image-box 序列是空的：`print/models.rs:39` 的 `EMPTY_IMAGE_DENSITY` 写成 `(2010,0120)`（那是 MinDensity，US），`print/models.rs:43` 的 `REFERENCED_IMAGE_BOX_SEQUENCE` 写成 `(2010,0500)`（那是 ReferencedFilmSessionSequence），二者正确值分别是 `(2010,0110)` 与 `(2010,0510)`。

另外两个**会渲染出错误像素**但严重度次一级的问题：RGB 与 YBR 两条路径的 bit-depth 契约互相矛盾（§2.7，实测 12-in-16 的 RGB 灰色 128 渲染成 7），以及 palette 索引不做 high-bit 归一化（§2.8）。

---

## 1. 功能缺失（对照 fo-dicom）

图例：✅ 已实现 ｜ 🟡 部分实现 ｜ ❌ 缺失 ｜ ⛔ 有意不实现

> 判定口径：**以功能是否达成为准**，不要求与 fo-dicom 同名的 .NET 类/接口/结构一一对应。Rust 按其生态
> 惯例实现（具体类型而非 `IPixelData`/`IImage` 接口等）；功能等价即视为已实现，.NET 专有抽象在 Rust
> 无必要者标“不适用/有意不实现”。

### 1.1 像素访问与帧切分（fo-dicom `DicomPixelData` / `PixelDataFactory`）

| fo-dicom 能力 | 状态 | radicom 对应符号 | 证据/行为差异 |
|---|---|---|---|
| 帧访问、offset table 解析、单片段/一片段一帧判定 | ✅ | `PixelData::get_frame`（`pixel/mod.rs:210`）、`encapsulated_frame`（:244）、`frame_from_offset_table`（:271） | 四条判定规则齐备 |
| 零拷贝帧窗口 | ✅ | `native_frame`（:218）、`RangeBuffer`、`CompositeBuffer` | 比 fo-dicom 更省 |
| `width()`/`height()` 属性 | ✅ | `pixel/mod.rs:126-132` | 从 `(0028,0010/0011)` 读取，真实文件返回正确值（§2.1 已修复） |
| 1-bit 像素数据（`SingleBitPixelData`） | ✅ | `render/mod.rs` 单比特解包 | 按 MSB-first 解包为黑/白，`single_bit_frames_unpack_msb_first_as_black_and_white` 覆盖 |
| 32-bit 灰度（`GrayscalePixelDataU32/S32`） | ✅ | `render/mod.rs` 32-bit 通路 | 有符号/无符号 32-bit 经窗口渲染，64-bit 报明确错误；均有测试 |
| 逐通道直方图 | `IPixelData.GetHistogram`（.NET 接口） | ✅（按功能） | `render/math.rs` `Histogram` + `frame_stored_values` | 直方图由样本流式计算；"逐通道"是 .NET 接口的便利方法，Rust 侧按需对指定通道取样即可，不构成功能缺口 |
| 渲染缓存 | `CacheType{None,PixelData,LookupTables,Display}`（.NET 缓存枚举） | ⛔ 不适用 | 无对应符号 | Rust 侧不内建跨帧缓存；调用方可用 `from_dataset_frame_parts` 复用已解析 LUT/参数，属性能取舍而非功能缺口 |
| palette LUT 16-bit-allocated 取高字节 | ✅ | `pixel/mod.rs:457-487` `read_lut` | 行为与 `DicomPixelData.cs` `GetPaletteColorLUT` 的 `offset=1` 一致 |
| palette LUT 长度校验 | ✅ | `read_lut`（`pixel/mod.rs:478`） | 描述符/数据缺失、bits 非 8/16、entry 数超 2^BitsStored、数据长度不足都会返回错误（不再静默补零） |
| 未压缩帧尺寸（1-bit / 4:2:2 / 4:2:0 特例） | ✅ | `uncompressed_frame_size`（`pixel/mod.rs:181-205`） | 算术正确；width/height 取自 `(0028,0010/0011)`（§2.1 已修复） |

### 1.2 光度解释与色彩转换（fo-dicom `PhotometricInterpretation` / `PixelDataConverter`）

| fo-dicom 能力 | 状态 | radicom 对应符号 | 证据/行为差异 |
|---|---|---|---|
| PI 枚举与 color space 属性 | ✅ | `pixel/photometric.rs:12-26`、`ColorSpace`（:44-48） | 12 个变体 + `Other` |
| `YbrFullToRgb` / `YbrFull422ToRgb` / `YbrPartial422ToRgb` | ✅ | `ybr_full_to_rgb` / `ybr_partial_to_rgb`（`render/mod.rs`） | YCbCr→RGB 已抽为可复用的纯函数并单独单测；4:2:2 布局解包仍内联于 `render_ybr_frame` |
| 位序 / planar 转换 | `ReverseBits` / `InterleavedToPlanar24` / `PlanarToInterleaved24` | ✅（按功能） | `render/pixel_converter.rs` `reverse_bits`；planar↔interleaved 在 `render/mod.rs` 内联 | `reverse_bits` 用于单比特/MONOCHROME1 通路；planar 交错转换在 YBR/RGB 渲染路径内联完成，无需独立 .NET 式 helper |
| YBR_FULL 的 planar configuration 1 | ✅ | `render/mod.rs:1674-1776` | 4:4:4 planar 先按三平面重组再转 YBR；仅下采样 YBR 要求交织（标准限制） |
| HSV / ARGB / CMYK / YBR_ICT / YBR_RCT 渲染 | ⛔ | `render/mod.rs:954` `_ =>` 报错 | 解析支持，渲染不支持 |
| YBR_ICT / YBR_RCT 原生（未压缩）数据 | ⛔ | 同上 | 这两种 PI 按标准只用于压缩语法，合理 |

### 1.3 灰度渲染管线（fo-dicom `ILUT` 家族 / `GenericGrayscalePipeline`）

| fo-dicom 能力 | 状态 | radicom 对应符号 | 证据/行为差异 |
|---|---|---|---|
| Modality LUT Sequence（`(0028,3000)`） | ✅ | `ModalityLut`（`render/mod.rs`） | 描述符长度/位数校验、VR=SS 有符号处理已修复 |
| Rescale Slope/Intercept | ✅ | `render/mod.rs:204-205,317-318` | |
| VOI LUT Sequence（`(0028,3010)`） | ✅ | `VoiLut`（`render/mod.rs`） | 长度/VR/位数缩放与负数描述符取绝对值已覆盖（`a_negative_voi_lut_entry_count_uses_its_absolute_value`、`a_voi_lut_descriptor_first_mapped_is_signed_ss`） |
| VOI LINEAR / LINEAR_EXACT / SIGMOID | ✅ | `map_rescaled`（`render/mod.rs`） | 公式对齐；`WindowWidth==1` 已按 fo-dicom 阈值特判，`w<1` 走 `linear_exact`，无除零 |
| `OutputLUT`（ColorMap） | ✅ | `apply_colormap`、`ColorTable` | 仅在灰度渲染路径应用（`show_overlays` 前），输入为 BGRA 灰度（三通道相等），故索引 `px[0]` 即灰度值 |
| `InvertLUT`（MONOCHROME1） | ✅ | `render/mod.rs:234-237,451-453,471-473` | |
| `PrecalculatedLUT`（整数预计算表） | ✅ | `render/mod.rs` 预计算窗口表 | 窗口按存储值区间一次性预算成表再查表（`the_precalculated_window_matches_per_pixel_mapping`） |
| XA/XRF 不施加 Modality LUT | ✅ | `is_xa_xrf`（:503-513）、`:242-243` | 与 Clunie 指引一致 |
| VOI LUT Sequence 优先于 WindowCenter/Width | ✅ | `from_dataset_frame`（:207-227） | 顺序正确 |
| `from_histogram` 剔除 padding | ✅ | `from_histogram`（`render/mod.rs:312`） | 计算 bin 范围与直方图时都跳过 `(0028,0120)` 并 `Histogram::clear(padding)`，与 fo-dicom 一致 |
| Padding 感知的 `GetMinMax` | ✅ | `from_min_max` 过滤 `(0028,0120)`；`frame_stored_values` 做 high-bit 归一化 | `high_bit_normalisation_drives_the_derived_min_max_window`、`grayscale_normalises_high_bit_padding_before_windowing` 覆盖 |

### 1.4 图像对象与几何

| fo-dicom 能力 | 状态 | radicom 对应符号 | 证据/行为差异 |
|---|---|---|---|
| 图像对象抽象 | `RawImage` / `IImage` / `IImageManager`（.NET 接口） | ⛔ 不适用 | `render/mod.rs` `RawImage`（具体类型） | Rust 用具体结构 + `&[u8]`/`RawImage` 传值，不必复刻 .NET 的 `IImage`/`IImageManager` 接口；导出/缩放/colormap 能力均已具备 |
| PNG / BMP 导出 | ✅ | `to_png`（:657）、`to_bmp`（:693）、`save`（:736） | 自实现 CRCs，无外部依赖 |
| `DicomImage`：帧/窗口/缩放/colormap/overlay 开关 | ✅ | `render/mod.rs:997-1163` | |
| `DicomImage.Scale` | ✅ | `scaled`（:625-650） | |
| `SpatialTransform`（旋转/翻转/pan） | ✅ | `render/spatial.rs` | 与 fo-dicom 一致：只有 90/180/270 有意义，其余角度忽略并 `warn!`（不静默取整）；flip/pan 正确 |
| `FrameGeometry`：ImagerPixelSpacing / PixelSpacing | ✅ | `render/geometry.rs:55-80` | |
| `FrameGeometry`：`NominalScannedPixelSpacing` `(0018,2010)` | ✅ | `render/geometry.rs:82` | 作为 PixelSpacing/ImagerPixelSpacing 之后的第三优先级 |
| enhanced multi-frame 的 per-frame 几何（Functional Group） | ✅ | `render/geometry.rs:59-101` `from_dataset_frame` | 读 Per-Frame `(5200,9230)`→Pixel Measures `(0028,9110)`，回退 Shared `(5200,9229)` |
| `DicomIconImage` / `IconImageSequence` | ✅ | `render/icon.rs` | PI 限定 MONOCHROME1/2/PALETTE COLOR，与 fo-dicom 一致；`pixel.width()`/`height()` 现从数据集读取（§2.1 已修复），真实文件可用 |
| `VolumeData` / `Stack` / MPR | ✅ | `render/reconstruction.rs` | 功能存在；`read_grayscale` 依赖的 `pixel.width()` 已修复 |
| `Mathematics`（Point/Vector/Matrix/Histogram/BitList） | ✅ | `render/math.rs:1-615` | `BitList` 与 fo-dicom 同为 LSB-first（:315） |

### 1.5 打印（对照 `FO-DICOM.Core\Printing\`）

| fo-dicom 能力 | 状态 | radicom 对应符号 | 证据/行为差异 |
|---|---|---|---|
| `FilmSession`（copies/priority/label/medium/destination） | ✅ | `print/models.rs:52-124` | 五项属性均提供读写方法 |
| `FilmBox`（display format / film size / trim） | ✅ | `print/models.rs:105-204` | |
| `ImageDisplayFormat` STANDARD/ROW/COL/SLIDE/SUPERSLIDE/CUSTOM | ✅ | `print/layout.rs` | STANDARD/ROW/COL 精确网格；CUSTOM 由厂商定义，布局返回空（fo-dicom `FilmBox.Initialize` 同样不处理 CUSTOM/SLIDE）；SLIDE/SUPERSLIDE 为单格 |
| `ImageBox`（position/polarity/magnification/smoothing/min-max density/requested size） | ✅ | `print/models.rs` | 所有属性均有读写；渲染应用 `REPLICATE` 最近邻、其余用双线性、`RequestedImageSize` 控制宽度。Smoothing/Density 属打印机 SCP 参数，fo-dicom Core 亦仅建模不渲染 |
| `PresentationLut` / `PresentationLUTShape` | ✅ | `print/presentation_lut.rs:20-183` | 已支持 Shape、Sequence、Descriptor/Data 解析与输出映射；Descriptor entry count 按 VR 解码 |
| `FilmBox.Initialize()` 默认值填充 | ✅ | `initialize_image_boxes`（`print/models.rs`） | tag 常量委托 `Tag::*` 生成表（`(2010,0510)` 正确），不再写错序列；见 §2.6 |
| 胶片渲染（`FilmBoxExtensions.Print`） | ✅ | `print/render.rs` | 画布/裁切/极性/呈现 LUT 可用；`REPLICATE` 最近邻、其余双线性，`RequestedImageSize` 控制宽度。Smoothing/border density 为 SCP 侧参数（Core 不渲染） |
| `Save` / `Load` / `Print` / 打印 SCU | ⛔ | 无 | 网络打印不在范围，视为有意不实现 |

---

## 2. 正确性缺陷

### 2.1 【高】`width()`/`height()` 用 `u32` 读 VR=US 标签 —— 真实文件的渲染与取帧全部失效

**位置**：`pixel/mod.rs:126-132`；根因 `model/element.rs:192-206`；下游 `pixel/mod.rs:181-205`、`:218-241`、`render/mod.rs:899-902`、`:1036-1041`、`:1181`、`:1306`。

```rust
// pixel/mod.rs:126
pub fn width(&self) -> u32 {
    self.dataset.get_or(COLUMNS, 0u32)   // COLUMNS = (0028,0011), VR = US
}
```

`Dataset::get_or` 是 `self.get(tag).unwrap_or(default)`（`model/dataset.rs:132-134`），把所有读取错误吞成默认值。而 `get` 对解析出的 `ElementValue::Bytes` 走 `read_le_unit`：

```rust
// model/element.rs:193
let unit = e.vr().unit_size().max(T::SIZE);   // US: 2, u32: 4 -> 4
...
if end > data.len() { return Err(IndexOutOfRange { index: i, count: data.len() / unit }); }
```

US → u32 时 `unit = 4`，但单值 US 只有 2 字节，于是 `IndexOutOfRange{index:0, count:0}`，被 `get_or` 吞成 `0`。

**实测证据**（`ct.dcm` / `mr.dcm` 为 fo-dicom 自带数据）：

| 数据集 | 元素形态 | `try_get::<u16>` | `try_get::<u32>` | `width()` | `frame_size` | 原始像素字节 | `get_frame(0)` |
|---|---|---|---|---|---|---|---|
| 解析 `ct.dcm` | `Us / Bytes([0x00,0x02])` | `Some(512)` | `None` | 0 | `Ok(0)` | 524288 | **`Ok(0)`** |
| 解析 `mr.dcm` | `Us / Bytes([0x00,0x01])` | `Some(256)` | `None` | 0 | `Ok(0)` | 131072 | **`Ok(0)`** |
| 手搓（各测试的写法） | `Is / Str("4")` | `Some(4)` | `Some(4)` | 4 | `Ok(16)` | 16 | `Ok(16)` |

`render_frame()` 的后果：`render/mod.rs:899-902` 命中 `width == 0 || height == 0` → 全部真实文件报 `frame has zero width or height`。**只把 ROWS/COLUMNS 换成字符串路径重新插入（不动其他任何代码与数据），`ct.dcm` 立刻渲染出 512×512**——证明这是唯一根因，不是多个缺陷叠加。

**第二个后果（更危险，静默）**：`uncompressed_frame_size()` 返回 `Ok(0)` 后，`native_frame()` 的越界检查失效：

```rust
// pixel/mod.rs:232
if offset + frame_size > buffer.size() {   // 0 + 0 > 524288 == false，不报错
    return Err(...);
}
let window = Arc::new(RangeBuffer::new(buffer, offset, frame_size));  // 0 长度，仍返回 Ok
```

即"零尺寸帧"绕过了库自己的边界保护，产生**成功返回的空帧**。这与 PS3.3 无关，纯粹是自洽性问题：`uncompressed_frame_size()` 遇到 width/height 为 0 时应返回错误，而不是 `Ok(0)`。

**正确行为**：`width()`/`height()` 读 `u16`（或让 `read_le_unit` 在 `T::SIZE > unit_size` 时按 unit 读再零扩展）；`uncompressed_frame_size()` 对 0 维度报错；`get_or` 不应把 `IndexOutOfRange`/VR 不匹配吐成默认值——这类错误对几何标签几乎总是 bug 而非缺席。

**下游同一根因**：`render/mod.rs:899`（灰度）、`:1040`（`pixel_info`→`width/height/number_of_frames`）、`:1181`（`read_frame_samples`）、`:1306`（YBR）、`render/icon.rs:55` 之后的图标渲染、`render/reconstruction.rs:337`（`width*height*samples` = 0）、`pixel/transcoder.rs`（逐帧尺寸 0，实测 `transcode explicit→implicit` 后 `get_frame(0) = Ok(0)`）。

### 2.2 【已修复】16-bit VOI LUT 输出按输出范围重标定（不再用 `>> 8`）

**位置**：`render/mod.rs:99-105`；相关 `render/mod.rs:58-85`。

```rust
pub fn map_byte(&self, value: f64) -> u8 {
    match self.at(value) {
        Some(v) if self.bits >= 16 => (v >> 8) as u8,   // <-- 固定缩放
        Some(v) => v.min(255) as u8,
        None => 0,
    }
}
```

PS3.3 C.11.2.1.1 明确 LUT 输出的数值范围是 `0..2^n−1`（n = 描述符第三值），而不是"必然铺满 0..65535"。fo-dicom 的处理是把 LUT 的**实际**输出区间喂给线性映射：`GenericGrayscalePipeline.cs:146-147` 用 `VOISequenceLUT.MinimumOutputValue/MaximumOutputValue` 构造 `VOILinearLUT(CreateLinearOption(bits, Min, Max))`，再由 `OutputLUT` 落到 0..255。因此 `>> 8` 只在 LUT 恰好覆盖 `[0, 65535]` 时正确。

**实测**（16-bit 描述符，数据 `[0, 1000]`，输入值 1）：

| LUT 数据 | 正确输出 | radicom |
|---|---|---|
| `[0, 65535]` | 255 | 255 ✅ |
| `[0, 1000]` | 255 | 255 ✅（已按输出范围重标定） |

窄区间 16-bit LUT（real-world 常见，例如把 12-bit 输入映射到 0..4095）会渲染成近乎全黑。

**而仓库测试把这个错误固化成了期望值**：`render/mod.rs:1521-1527`

```rust
#[test]
fn a_16_bit_voi_lut_scales_its_entries_down_to_a_byte() {
    // entry 0x8000 (32768) → output byte 0x80 (128), not clamped to 255
    ...
    assert_eq!(o.map(5.0), 0x80, "16-bit LUT entries scale by >>8");
}
```

注释直接写着 "scale by >>8"，即测试断言的是实现，不是标准。

### 2.3 【已修复】LUT 描述符/数据的长度、VR、符号处理

**位置**：`render/mod.rs:58-85`（`LutTable::from_item`）、`:64`、`:66`、`:77-82`；`pixel/mod.rs:457-487`。

1. **不做长度校验，静默补零**：`let lo = *data.get(i * 2).unwrap_or(&0)`（:78-79）。描述符声明 4 项、数据只有 2 项时，实测**不报错**，渲染出全 0（`px0=0 px1=0`）。fo-dicom `VOISequenceLUT.cs:102-104` 明确 `throw new DicomImagingException("Number of entries in VOI LUT Sequence do not match")`。
2. **不区分 VR**：描述符 `(0028,3002)` 的 VR 是 `US\SS`（`radicom-dict` 亦然），`[0]` 在 SS 语境下可以是负数，fo-dicom 取 `Math.Abs`（`VOISequenceLUT.cs:115`：`_nrOfEntries = Math.Abs(LUTDescriptor.Get<int>(0))`，注释写 "Sometimes negative number is defined and this makes no sense"）；数据 `(0028,3006)` 可以是 `OW`/`US`/`SS`，fo-dicom 分别按 `ushort`/`ushort`/`short` 解释。radicom 一律按 `u16::from_le_bytes` 读（:63、:80），SS 数据被当成无符号。
3. **实测 SS 描述符**：`entries=-3, first=-2` 曾被当作 `65533`，合法的三项表因此被拒绝；现已按 Descriptor 的 `SS` VR 取绝对值，回归测试 `a_negative_voi_lut_entry_count_uses_its_absolute_value` 覆盖该兼容行为。
4. `bits` 用 `>= 16` 判断（:80），而 PS3.3 说第三值只能是 8 或 16；8 当作"取低字节"、16 当作"取两字节"，语义上恰好可用，但缺少对其它值的拒绝。
5. `pixel/mod.rs:449` 的 `palette_first_mapped` 曾把 `(0028,1101)/(0028,1201)` 描述符 `[1]` 当作 `u16` 再转 `i32`，**永远非负**；现已按描述符 VR 读取，`SS` 使用有符号值，回归测试 `palette_descriptor_reads_a_signed_first_mapped_value` 覆盖 `-2` 起点。PS3.3 允许 SS 语义的负起点，fo-dicom `PaletteColorLUT` 同样保留该符号。

### 2.4 【高】overlay 的 `ImageFrameOrigin` 与 `OverlayOrigin` 行列顺序都读错

**位置**：`pixel/overlay.rs:66-67`；消费方 `render/mod.rs:798-800`、`:1167-1172`。

```rust
// pixel/overlay.rs:66-67
origin: (ds.get_or(Tag::new(group, 0x0050), 0i16), ds.get_or(Tag::new(group, 0x0051), 0i16)),
frame_origin: (ds.get_or(Tag::new(group, 0x0052), 0i16), ds.get_or(Tag::new(group, 0x0053), 0i16)),
```

按 `radicom-dict` 与 PS3.3：

| tag | 真实含义 | radicom 当成 |
|---|---|---|
| `(60xx,0050)` | OverlayOrigin，**SS VM2，顺序是 row\column** | `origin.0` → x |
| `(60xx,0051)` | **ImageFrameOrigin，US VM1** | `origin.1` → y |
| `(60xx,0052)` | OverlayPlaneOrigin（US VM1，**已退休**） | `frame_origin.0` |
| `(60xx,0053)` | **不存在** | `frame_origin.1` |

**实测 1（帧定位）**：6 帧图像，overlay `NumberOfFramesInOverlay=2`、`ImageFrameOrigin=3`。按 PS3.3 C.9.2.1.4 应出现在第 3、4 帧（0-based 2、3）。实测 `frame_origin()` 读出 `(0,0)`，overlay 被画在**第 0、1 帧**。

**实测 2（位置）**：4×3 图像，`OverlayOrigin = 2\3`（row 2、column 3）。按 C.9.2 左上像素是 `1\1`，overlay 应在 `x=1, y=2`… 依 radicom 自身"1-based → -1"约定应为第 2 列第 3 行。实测 overlay 画在 **(x=2, y=0)**，即行列互换且行偏移错位：

```
A2 row 0: . O . .      <- 期望 row 2, col 1
A2 row 1: . . . .
A2 row 2: . . . .
```

`render/mod.rs:799-800` 的 `base_x = origin_x - 1` 只对"x 已正确解析"成立，源头错了两处，下游无可救。

### 2.5 【高】嵌入式 overlay 位平面按 MSB-first 打包（应为 LSB-first）

**位置**：`pixel/overlay.rs:177`（注释）、`:194-196`；消费方 `render/mod.rs:804`。

PS3.5 §8.1.2（已逐字核对）：overlay data 的"第一个 bit of an Overlay Plane is encoded in the least significant bit"。fo-dicom `SingleBitPixelData` 用 `new BitArray(input)` 展开，同样是 LSB-first（`PixelData.cs:400-410`）。

radicom 写的是：

```rust
// pixel/overlay.rs:195
out[o + x / 8] |= 0x80 >> (x % 8);
```

**实测**：8×1、16-bit 像素、overlay bit position 12、只有第一个像素置位 → `extract_embedded` 返回 `0b10000000`，而标准要求 `0b00000001`。整条 overlay 会水平镜像。

`render/mod.rs:804` 的读取端 `bits[...] & (0x80 >> (ox % 8))` 与打包端**自洽**，所以两侧一起错，测试看不出来；但和任何符合标准的文件（以及 fo-dicom）都不兼容。

### 2.6 【已修复】print 模块 tag 常量已委托生成表

**位置**：`print/models.rs:39`、`:43`；消费方 `:178-203`、`print/render.rs:70`。

```rust
/// (2010,0120) Empty Image Density.
pub const EMPTY_IMAGE_DENSITY: Tag = Tag::new(0x2010, 0x0120);      // 实际是 MinDensity (US)
pub const REFERENCED_IMAGE_BOX_SEQUENCE: Tag = Tag::new(0x2010, 0x0500); // 实际是 ReferencedFilmSessionSequence
```

权威值（`radicom-dict` 字典与 PS3.3 C.13.2）：

| 常量 | radicom 值 | 正确值 |
|---|---|---|
| `EMPTY_IMAGE_DENSITY` | `(2010,0120)` = MinDensity（US） | `(2010,0110)` EmptyImageDensity（CS） |
| `REFERENCED_IMAGE_BOX_SEQUENCE` | `(2010,0500)` = ReferencedFilmSessionSequence | `(2010,0510)` ReferencedImageBoxSequence |

后果：`initialize_image_boxes`（:179）把 image box 写进 FilmSession 引用序列的位置，`image_box_count`/`image_boxes`（:190-203）也从那里读回来——自身读写自洽，所以 9 个单元测试全绿；但**任何真实的 FilmBox 都不含这个序列**，`print/render.rs:70-71` 取到空列表，渲染出一张全背景胶片。反向也危险：真实 FilmBox 的 `(2010,0500)`（嵌套的 film session 引用）会被误当作 image box。

顺带：`print/models.rs:45` 把 `(2020,0010)` 命名为 `IMAGE_POSITION`，与 `(0020,0030)` 同名不同义，易误用。

### 2.7 【已修复】RGB 与 YBR 两条路径的 bit-depth 契约统一

**位置**：`render/mod.rs:1214-1231`（RGB 走 `BitDepth::normalize` + to8）vs `:1314-1320`（YBR 直接 `>> 8`）。

```rust
// render/mod.rs:1318  YBR
(u16::from_le_bytes([raw[byte_index], raw[byte_index + 1]]) >> 8) as f64
```

RGB 用 `BitDepth::new(bits, stored, high_bit, false)` 归一化再做 `>> down` / `<< up`（:1221-1231）；YBR 完全无视 `BitsStored`/`HighBit`，固定取高字节。

**实测**（1×1，中性灰 / 中性 YCbCr，期望 `[128,128,128,255]`）：

| 深度元数据 | RGB | YBR_FULL |
|---|---|---|
| 16/16/15 | ✅ 128 | ✅ 128 |
| 12/12/**11**（低位对齐） | ✅ 128 | ✅ 128（`ybr_normalises_bits_stored_for_low_aligned_16_bit_data`） |
| 12/12/**15**（高位对齐） | ✅ 128 | ✅ 128 |
| 10/10/**9** | ✅ 128 | ✅ 128 |

即**低位对齐的 12-in-16 / 10-in-16 YBR 全部渲染错误**（绿屏，因为 Cb/Cr 被右移成 0）。fo-dicom 的 `GrayscalePixelDataU16` 构造器统一做 `shiftLeft/shiftRight`（`PixelData.cs` U16 类构造函数），色彩路径同样适用。

### 2.8 【中】palette 索引不做 high-bit 归一化

**位置**：`render/mod.rs:1268-1274`。

```rust
for &index in &samples[..count] {
    let entry = index as i32 - first_mapped;   // 直接用原始样本当索引
```

`read_frame_samples`（:1177-1198）只做字节拼装，不归一化 `BitsStored`/`HighBit`。**实测**：16 项 palette，索引 3 的条目是 `(200,100,50)`；像素字段为高位对齐的 12-in-16 时，索引读成 3072 → 越界 → 钳到最后一格 → 输出 `[3,2,1]`。低位对齐与 16-bit 正常。RGB 路径（§2.7）做了归一化，palette 没做，三条路径三种契约。

### 2.9 【中】`frame_stored_values` 不归一化 high-bit，导致 min/max 与直方图窗口全错

**位置**：`render/mod.rs:546-569`。

`render_grayscale_with_options`（:911-912）用 `BitDepth::normalize`，但 `frame_stored_values` 直接把原始 `u16` 当值（:561-567）。它供给 `from_min_max`（:304）与 `from_histogram`（:274）。

**实测**：16/12/15（高位对齐），真实值范围 `0x100..0xF00`（256..3840）。`from_min_max` 得到 `center=32768, width=57344`（来自 0x1000 与 0xF000 的原始位型），正确应为 `center=2048, width=3584`。结果渲染出全黑（`px0=0 px1=0`）。

### 2.10 【中】错误被静默吞掉，渲染失败表现为"空白"而非诊断

| 位置 | 行为 |
|---|---|
| `render/mod.rs:985-988` | `if image.composite_overlay(...).is_err() { continue; }` —— overlay 加载失败无声跳过 |
| `print/render.rs:77` | `Err(_) => continue` —— 单元格渲染失败退化为背景，看起来像"这张图是黑的" |
| `model/dataset.rs:132-134` | `get_or` 吞掉 VR/索引错误（§2.1 的放大器） |

单条可辩（"坏 overlay 不该毁掉整张图"），但三者叠加 + §2.1 的零尺寸帧，使"静默产出错误图像"成为本模块的系统性特征。

### 2.11 【中】overlay 合成是"覆盖"而非 fo-dicom 的 OR

**位置**：`render/mod.rs:802-820`，赋值在 `:815-818`。

fo-dicom `OverlayGraphic.cs:93` 是 `pixels[p] |= _color`（按位 OR 合成，保留底图的其它位）。radicom 直接 `self.bytes[i] = color.b ...`。

**实测**：底图灰度 60 + 洋红 overlay → radicom `[255,0,255,255]`；fo-dicom 语义为 `[255,60,255,255]`。单色 overlay 下差异不大，但一旦 overlay color 不是纯色（或需要叠加多层 overlay），底图信息被抹掉。

### 2.12 【中】嵌入式 overlay 只处理 frame 0，且忽略 Origin 偏移

**已修复**：

- `OverlayData::extract_embedded_frame(ds, group, frame)` 现按帧切取像素窗口（`frame * rows * cols * unit`），
  `load_frame` 把当前渲染帧传进去；`RawImage::composite_overlay_at_frame` 由 `composite_overlays`
  按 `render_frame` 的帧号调用，因此多帧图像的 overlay 取自正确的帧。回归测试
  `embedded_overlay_extracts_the_plane_from_the_requested_frame` 覆盖 frame 0/1 与越界帧。
- `OverlayOrigin` 的行列语义与 1-based 偏移已在 §2.4 修复，`extract_embedded` 现在按
  `ox = max(0, OriginColumn-1)`、`oy = max(0, OriginRow-1)` 定位。
- bit-position 合法性判据已改为 fo-dicom 的 `BitPosition <= HighBit && BitPosition > HighBit - BitsStored`
  （见本节上文代码与 `bit_position_inside_stored_bits_is_rejected`）。

### 2.13 【已修复】`WindowWidth == 1` 已按 fo-dicom 阈值特判

**位置**：`render/mod.rs:458-463`。

fo-dicom `VOILinearLUT`（`VOILUT.cs:129-141`）对 `WindowWidth == 1` 特判：`value < WindowCenterMin05 ? MinimumOutputValue : MaximumOutputValue`。radicom 只在 `w < 1.0` 时切到 `linear_exact`，`w == 1.0` 落入通用公式 `(v-(c-0.5))/(w-1.0)`，分母为 0。整数输入时靠 `±inf` 被 clamp 侥幸得到正确结果（实测像素 0/99/100/200 → `[0,0,255,255]`，与 fo-dicom 一致），但 `v == c-0.5` 时得到 `NaN` → `0`（实测 `map(39.5)` 且 `c=40,w=1` → `0`，fo-dicom 为 `255`）。属边界缺陷，非常见路径。另 `Sigmoid` 在 `w == 0` 时用 `1.0` 顶替（:467），fo-dicom 会除以 0 得 `inf`；两者都不是好行为，但契约不同。

### 2.14 【已修复】非 90° 倍数的旋转被忽略并告警（与 fo-dicom 一致）

**位置**：`render/spatial.rs:58-75`。

```rust
a if a <= 90 => rotate_90(...)
a if a <= 180 => rotate_180(...)
```

`rotation = 45` 会得到 90° 的图，无任何提示。fo-dicom 桌面端 `WinFormsImage.GetRotateFlipType`（`Platform\FO-DICOM.Imaging.Desktop\WinFormsImage.cs:123-128`）只匹配 `90/180/270`，其它值走 `RotateNone`。属"radicom 独有行为"，应至少 `debug_assert` 或只接受直角。

### 2.15 【低】文档与签名不符

`render/mod.rs:1174-1180`：

```rust
/// Returns `(samples, width, height, (samples_per_pixel, planar_configuration))`.
fn read_frame_samples(...) -> Result<(Vec<u16>, u32, u32), Error> {
```

注释承诺四元组，实际三元组。

---

## 3. 性能问题

基准：本机 `cargo --release`，单线程，`render_frame` 于 16-bit MONOCHROME2 帧。实测 **162 Mpx/s**（512² 1.6 ms，2048² 25.9 ms）；作为对照，fo-dicom 的 `Render` 全部包在 `Parallel.For` 里（`PixelData.cs` 各 `Render` 重载、`OverlayGraphic.Render`）。

### 3.1 【明显】整个 crate 没有并行化

grep `rayon|par_iter|into_par_iter|scope(` 于 `crates/radicom/src/**/*.rs` → **0 命中**（`rayon` 只是 dev-dependency，`crates/radicom/Cargo.toml:29`）。每个像素循环都是串行：

- `render/mod.rs:915-928`（灰度主循环）
- `render/mod.rs:1241-1246`（RGB）、`:1268-1276`（palette）、`:1353-1377`（YBR）
- `render/mod.rs:802-820`（overlay）
- `render/mod.rs:610-618`（colormap）
- `print/render.rs:146-152`（blit）

按行切分即可并行（`Output` 各通道独立），无需改算法。这是最省力的一档收益。

### 3.2 【明显】没有 `PrecalculatedLUT`，窗口公式每像素重算

fo-dicom 把整条 LUT 链包进 `PrecalculatedLUT(_lut, BitDepth.MinimumValue, BitDepth.MaximumValue)`（`GenericGrayscalePipeline.cs:168`），一次算好 0..65535 的表，之后每像素只做整数查表。radicom 每像素走 `GrayscaleRenderOptions::map`（`render/mod.rs:437-475`）：分支 + f64 乘除 + `round()` + `clamp()`，且 `VoiFunction` 的 `match` 在热循环内。

### 3.3 【已测量】LUT 每次渲染都重建

`from_dataset_frame`（:209-212）每次都解析 `(0028,3010)` 序列并 `Vec::with_capacity(count)` 复制整张表。实测：**1×1 帧 + 65536 项 VOI LUT → 0.066 ms/次渲染**，同一帧无 LUT 时 0.002 ms/次。即一次渲染的 97% 时间花在重建一张与上一帧完全相同的表上。`DicomImage` 也无任何缓存（§1.1 `CacheType` 缺失）。

### 3.4 【明显】每次渲染 clone 整个 `Dataset`

`render/mod.rs:887`、`:1016`、`:1037`、`:1202`、`:1252`、`:1291`、`:547` 均为 `dataset.clone()`（注释说明是为绕过 `PixelData` 的可变借用）。`Dataset` 是 `BTreeMap`（`model/dataset.rs:32`）+ `Arc<dyn Buffer>`，元素本身不深拷贝，但**每次渲染重建一遍 BTreeMap**。2048² 帧本身 25 ms，可忽略；1×1 帧时就是全部开销。

### 3.5 【已测量】`DicomImage` 的几何访问器每次 clone 数据集

`pixel_info()`（:1036-1041）clone + `PixelData::from_dataset`，然后 `width()`/`height()`/`number_of_frames()`/`is_grayscale()` 各调一次（:1043-1057）。实测连续调 `width()+height()+number_of_frames()` 于 2048² 数据集：**2.12 µs/组**。UI 每帧刷几何信息时是纯浪费。

### 3.6 【明显】`scaled()` 做两次全图格式转换

`render/mod.rs:625-650`：先 `BGRA → RGB` 建一个 `Vec`（:634-637），双线性重采样（:638-644），再 `RGB → BGRA` 建第二个 `Vec`（:645-648）。两次全图分配 + 两次全图遍历，中间格式与 `bilinear_color24` 的入参耦合。改成直接按 4 通道采样可省一半。

### 3.7 【明显】打印路径的三层全图拷贝

`print/render.rs:97-103` `Canvas::filled` 逐像素 `extend_from_slice(&[b,g,r,0xFF])`（可用 `resize`/`chunks_exact_mut` 一次填）；`:141-142` 每个单元格 `to_rgb24` 再 `bilinear_color24`（两次全图分配）；`:146-152` 逐像素调 `Canvas::set`，每次都做四次 `i64` 边界比较（:105-113）。一张 A4@300dpi 是 2480×3508 ≈ 8.7 Mpx，这些常数不可忽略。

### 3.8 【已修复】transcoder 每帧一次 `to_vec()`

`pixel/transcoder.rs:149-158` `read_frames`：`ds.clone()` + 循环内 `pd.get_frame(i)?.data().to_vec()`——**把零拷贝帧窗口立刻物化成独立 `Vec`**，一次性把所有帧读进内存（多帧 cine 时峰值 = 整份像素数据 ×2）。`read_frame`（:161-164）与 `decode_frame`（:119-144，两次 `ds.clone()`）同理。`decode_frames`（:167-187）再 clone 一次源与解码结果。

### 3.9 【可疑，未测量】`frame_stored_values` 把整帧变成 `Vec<f64>`

`render/mod.rs:546-569`：16-bit 512² 帧 → 262144 个 `f64` = 2 MB 中间数组，只为算 min/max 和直方图。可直接在 `u16` 上滚动求极值。

---

## 4. 测试审查

### 4.1 【最重要】假绿：全部渲染测试用"手搓数据集"，从不让真实文件走字节路径

这是本模块测试体系的核心缺陷，也是 §2.1 能在整个测试套件全绿的情况下存在的原因。

**测试的构造方式**（`pixel/mod.rs:556-570`）：

```rust
fn mono_dataset(width: u32, height: u32, bits: u16, frames: u32) -> Dataset {
    ds.insert_value(ROWS, height);      // height: u32
    ds.insert_value(COLUMNS, width);    // width: u32
```

`insert_value` 走 `IntoElement::preferred_vr()`，`u32` 的默认 VR 是 **IS**（`model/element.rs:551-556`），于是元素被存成 `Str("512")`；`get::<u32>` 走字符串解析分支，成功。而解析器产出的元素是 `Bytes([0x00,0x02])`，`get::<u32>` 走 `read_le_unit`，失败。**测试与生产走两条不同分支。**

同型构造在 `render/` 下至少 10 处：`render/mod.rs:1392-1393`、`:1843-1844`、`:1914-1915`、`:2287-2288`，`render/icon.rs:107-108`，`print/render.rs:193-194`。集成测试（`crates/radicom/tests/*.rs`，仅 `cli_dump/corpus/fuzz/goldens`）grep `render_frame|PixelData::from_dataset|get_frame(` → **0 命中**。

修正方向不是"再加一个测试"，而是**至少有一个渲染测试从 `tests-data/` 里的真实 `.dcm` 读入后渲染**（`corpus.rs` 已有遍历真实文件的骨架）。

### 4.2 同义反复 / 断言实现而非行为

| 测试 | 位置 | 问题 |
|---|---|---|
| `a_16_bit_voi_lut_scales_its_entries_down_to_a_byte` | `render/mod.rs:1521-1527` | 断言 `0x8000 → 0x80`，注释写 "scale by >>8"；把 §2.2 的错误固化为期望值。正确断言需给一个非满量程的 16-bit LUT（如 `[0,1000]`）并检查 255 而非 3 |
| `overlay_properties_map_the_group_tags` | `pixel/overlay.rs:394-406` | 用 `(0x6002,0x0050)=3`、`(0x6002,0x0051)=4`，再断言 `origin()==(3,4)`——断言的是自己的赋值，不涉及行列语义，也不覆盖 `ImageFrameOrigin`（§2.4 因此逃过） |
| `extract_overlays_normalizes_the_group` | `pixel/overlay.rs:374-392` | 输入字节 `0x80`、断言位平面非空，配合 MSB-first 打包自洽；换成标准 LSB-first 文件即失败，但测试无法发现（§2.5） |
| `frame_size_for_monochrome` / `frame_size_for_rgb_and_subsampled_ybr` | `pixel/mod.rs:589,600` | 手搓 `mono_dataset`，width/height 走字符串路径，永远非 0（§2.1） |

### 4.3 弱断言（stub 也能过）

| 测试 | 位置 | 断言内容 | 为什么弱 |
|---|---|---|---|
| `a_sixteen_bit_frame_renders_to_a_bgra_image` | `render/mod.rs:1670-1682` | 仅 `lum(pixels[3]) >= lum(pixels[0])` | 任何单调映射（含 `v>>8`、含恒等）都通过；无法发现 §2.2/§2.7 |
| `an_overlay_origin_outside_the_image_is_clipped_not_panicking` | `render/mod.rs:2342-2349` | 只断言 `image.width()==4` | 完全不画 overlay 的实现同样通过（§2.4 的错位检测不到） |
| `rendering_produces_a_canvas_of_the_film_size` | `print/render.rs:228-234` | 只断言画布 210×297 | 从不 blit 任何单元格的实现同样通过 |
| `the_background_uses_the_empty_density` | `print/render.rs:236-241` | 只查左上角一个像素 | 背景正确但内容全空也通过（§2.6 检测不到） |
| `trim_draws_a_border` | `print/render.rs:243-249` | 只查 (0,0) | 1px 边框 vs fo-dicom 的 `100*2/25.4`（约 7.9px）不区分 |

### 4.4 未覆盖的行为

- **真实文件渲染（最重要，见 §4.1）**：没有任何测试从磁盘读 `.dcm` 再渲染/取帧。
- **`uncompressed_frame_size` 的零维度输入**：无测试断言 `Ok(0)` 应改为错误。
- **palette：16-bit 描述符 + 非 8-bit 数据、短 LUT 数据**：`pixel/mod.rs:712-830` 覆盖了 8-bit、16-bit-allocated 高字节、6 字节描述符、0→65536，但**没有**短数据、`bits==16` 的真 16-bit 条目被截断的情形。
- **VOI LUT：长度不匹配 / SS 数据 / SS 描述符 / 非满量程 16-bit**：全缺（§2.2/§2.3）。
- **overlay：`ImageFrameOrigin` 多帧筛选、`OriginX/OriginY` 行列语义、LSB-first 打包**：全缺（§2.4/§2.5）。
- **`from_histogram` 的 padding 剔除**：`render/mod.rs:1608-1624` 只验宽度单调，不验 padding 被剔除（§1.3）。
- **`frame_stored_values` 的 high-bit 归一化**：`render/mod.rs:1815` 只测了渲染路径，没测窗口推导路径（§2.9）。
- **`SpatialTransform` 非直角旋转**：`spatial.rs` 7 个测试全是 90/180/270 与 flip/pan。
- **`geometry.rs` 的 `NominalScannedPixelSpacing` 与 functional group**：测试只有 axial/sagittal/coronal 分类。
- **`transcode_dataset` 的 lossy 标记**：`pixel/transcoder.rs:560-580` 直接调 `mark_lossy`，从不验证"转码后数据集被标记"。
- **print：`ImageBoxPosition` 与单元格的对应**：`print/models.rs:311-318` 只验编号，`print/render.rs` 的 `zip`（:71）无测试。
- **`render/pixel_converter.rs:51-78`** 的测试覆盖的是**生产代码从不调用的** `reverse_bits`/`interleaved_to_planar_24`/`planar_to_interleaved_24`（grep 确认仅本文件内引用）——测试有、覆盖有、功能是死的。

---

## 5. 优先修复顺序建议

1. **§2.1**（`u32` 读 US）：不修则整个 pixel/render/print 栈在真实数据上不可用。配套把 `uncompressed_frame_size()` 的 `Ok(0)` 改成错误，并给 `get_or` 加上"不吞 VR 不匹配"的区分。
2. **§4.1**（真实文件渲染测试）：与 1 同时做，否则同类问题会反复出现。一个从 `tests-data/*.dcm` 读入 → 渲染 → 断言已知像素的测试，价值高于现有全部渲染测试之和。
3. **§2.6**（print 两个 tag）：两行常量，修完全部打印功能才有意义。
4. **§2.4 / §2.5**（overlay 标签与位序）：改 3 个 `Tag::new` + 1 个移位方向，影响所有 overlay 的显示位置。
5. **§2.2 / §2.3**（LUT 长度、VR、16-bit 缩放）：需要重写 `LutTable` 的解析与输出映射；同时改掉 `render/mod.rs:1521` 那条把 bug 写成期望值的测试。
6. **§2.7 / §2.8 / §2.9**（bit-depth 归一化在三处不一致）：抽一个 `to_display8(sample, depth)` 走遍 RGB/YBR/palette/min-max 四条路径。
7. **§3.1 / §3.3**（并行化 + LUT 缓存）：在功能正确之后做，收益明确且互不冲突。
