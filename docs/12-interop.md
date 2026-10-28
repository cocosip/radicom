# 与真实对端联调（interop）

本文件说明如何把 radicom 与**真实 DICOM 对端**（如 fo-dicom 的 C-STORE SCP / Q/R SCP、
DCMTK、dcm4che、真实 PACS）做互操作验证。

> 说明：本仓库自带全面 SCU↔SCP 的 loopback 自测（`radicom-network` 约 250 个测试覆盖
> C-ECHO/STORE/FIND/MOVE/GET、扩展协商、User Identity、小 PDU 分片、C-CANCEL 等），
> 因此**不需要外部对端即可自测**。下面的工具用于**跨实现**互通的额外验收。

## 0. 离线自测（默认就运行）

`crates/radicom-network/tests/interop.rs` 里的 `self_interop_*` 用例**不需要外部对端**：
在本机 `127.0.0.1` 起一个 `DicomServer`，用 `DicomClient` 走真实 TCP 打过去，验证
C-ECHO 与 **真实文件跨 TCP 的 C-STORE→SCP→读回语义相等**（组长度归一化后逐一比对，
传输语法保持）。`cargo test --workspace --all-features` 即会跑它们。

## 1. 联调工具：`peer_interop` 示例

位置：`crates/radicom-network/examples/peer_interop.rs`

功能：可选的 C-ECHO；可选 C-STORE（发一个文件）；可选 C-FIND（QR）。
**每个阶段用各自的关联**，所以不支持 Verification 的 C-STORE-only SCP 不会影响后续操作。
C-ECHO 仅在它是唯一请求的操作时才算致命；任何*被请求的*操作失败则进程以非 0 退出。

```pwsh
# 仅 C-ECHO
cargo run -p radicom-network --example peer_interop -- `
  --peer 127.0.0.1:11112 --calling RADICOM --called DCM4CHE

# 只做 C-STORE（跳过 C-ECHO，适合 C-STORE-only SCP）
cargo run -p radicom-network --example peer_interop -- `
  --peer 127.0.0.1:11112 --calling RADICOM --called DCM4CHE --no-echo `
  --store tests-data\explicit-le\GH487.dcm

# C-FIND(STUDY)
cargo run -p radicom-network --example peer_interop -- `
  --peer 127.0.0.1:11112 --calling RADICOM --called DCM4CHE --no-echo `
  --find STUDY --patient-name "Doe*"
```

参数：
- `--peer host:port`（默认 `127.0.0.1:11112`）
- `--calling AE` / `--called AE`
- `--no-echo`：跳过 C-ECHO（对端不支持 Verification 时使用）
- `--store <file.dcm>`：对该文件做 C-STORE（SOP Class 取自 `(0008,0016)`）
- `--find PATIENT|STUDY|SERIES|IMAGE`：做 C-FIND；可选 `--patient-name`、`--patient-id`、
  `--study-uid`、`--accession` 作为匹配键

## 2. 联调用例（设为 CI 可选）

位置：`crates/radicom-network/tests/interop.rs`

正常 `cargo test` 会**跳过**（无对端，保持自洽）。设了 `RADICOM_PEER` 才真正连对端：

```pwsh
$env:RADICOM_PEER    = "127.0.0.1:11112"
$env:RADICOM_CALLING = "RADICOM"
$env:RADICOM_CALLED  = "DCM4CHE"
$env:RADICOM_STORE_FILE = "tests-data\explicit-le\GH487.dcm"   # 可选，附带 C-STORE
cargo test -p radicom-network --test interop -- --nocapture
```

## 3. 对照 fo-dicom 的验收项

| 服务 | 对端（fo-dicom） | 验收标准 |
|---|---|---|
| C-ECHO | Verification SCP | 状态 0x0000（C-STORE-only SCP 可能不支持，属正常） |
| C-STORE | C-STORE SCP | 状态 0x0000；对端收到的 dataset 与本端语义一致 |
| C-FIND | Q/R SCP | 返回匹配条数与返回键（PatientName/StudyInstanceUID 等）正确 |
| C-MOVE | Q/R SCP（+ 目的 AE） | 子操作计数 completed 正确、目的端有实体落入 |
| C-GET  | Q/R SCP | 反向 C-STORE 子操作 completed 正确、本端 reverse-store handler 收到实例 |

> C-MOVE/C-GET 需要一个"目的 AE / 反向存储"端，用 radicom 自身即可扮演
> （`collect_reverse_stores` + `DicomServer`），属于已有 loopback 覆盖范围。

## 4. 实机联调结果（fo-dicom 对端）

测于 2026-10-28，对端为 fo-dicom 实现的 SCP。

| 对端 | 端口 | Called/Calling | 结果 |
|---|---|---|---|
| C-STORE SCP | 172.16.2.24:31801 | NETGATE / NETPUSH | ✅ **C-STORE 状态 0x0000**（SC `1.2.840.10008.5.1.4.1.1.7` 与 CT `…1.1.2` 均成功）；Verification 不被接受（属正常） |
| Q/R SCP | 172.16.2.24:31803 | QRGATE / NETPUSH | ❌ A-ASSOCIATE-RJ：`result=1, source=1, reason=3`（called-AE-title-not-recognized）。尝试 `QRGATE/NETGATE/QRSCP/QR` 及 calling `NETPUSH/NETGATE/QRSCU/NETQR` 均同一拒绝 → 判为**对端 AE 配置问题**，待对端确认 Q/R SCP 的实际 AE 后复测 |

## 5. 联调暴露并修复的 DUL 一致性问题

这些 bug 在 loopback + 自产 goldens 下完全自洽而不可见，只有**真实严格对端**才会暴露；
已按 PS3.8 修正并有回归测试：

| # | 问题 | 标准 | 修复 |
|---|---|---|---|
| 1 | A-ASSOCIATE 的 Item 写成 `type(1)+length(4)` | PS3.8 §9.3.2：`type(1)+reserved(1)+length(2)`（16 位 Item-length；只有 P-DATA 的 PDV 用 32 位） | `push_item`/`item_header` 改为 4 字节头；P-DATA 不变 |
| 2 | item 值奇数长度未补偶 | §9.3：item-value 必须偶数长，奇数补一个 `0x00` | 编码端补偶，解码端去尾 NUL/空格 |
| 3 | A-ASSOCIATE-AC 重复编码 abstract syntax | §9.3.3：AC 只回 result + **一个**被接受的 transfer syntax，不含 abstract | 编码端 AC 省略 abstract；客户端按 context id 从 RQ 回填 |
| 4 | A-ASSOCIATE-RJ 体少 1 个 reserved 字节 | 标准 RJ 体为 `reserved(1)+result(1)+source(1)+reason(1)` | 编码补 reserved，解码跳过 reserved 并按 4 字节校验 |

## 6. 与 fo-dicom 语料的一致性（离线）

`tests-data/` 是从 fo-dicom MS-PL 测试数据导入的语料，`crates/radicom/tests/corpus.rs`
对其中 13 个样本做 **parse→write→parse 语义等价**往返；`goldens.rs`（dump/JSON/关联 PDU/
PDATA 十六进制）做逐字节/结构化对照。如需扩充，直接把 fo-dicom 生成的 `.dcm` 放入对应
`tests-data/<类别>/` 并在 `corpus.rs` 的 `CORPUS` 表登记即可。
