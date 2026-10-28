# 与真实对端联调（interop）

本文件说明如何把 radicom 与**真实 DICOM 对端**（如 fo-dicom 的 C-STORE SCP / Q/R SCP、
DCMTK、dcm4che、真实 PACS）做互操作验证。

> 说明：本仓库自带极全面 SCU↔SCP 的 loopback 自测（`radicom-network` 245 个测试覆盖
> C-ECHO/STORE/FIND/MOVE/GET、扩展协商、User Identity、小 PDU 分片、C-CANCEL 等），
> 因此**不需要外部对端即可自测**。下面的工具用于**跨实现**互通的额外验收。

## 0. 离线自测（默认就运行）

`crates/radicom-network/tests/interop.rs` 里的 `self_interop_*` 用例**不需要外部对端**：
在本机 `127.0.0.1` 起一个 `DicomServer`，用 `DicomClient` 走真实 TCP 打过去，验证
C-ECHO 与 **真实文件跨 TCP 的 C-STORE→SCP→读回语义相等**（组长度归一化后逐一比对，
传输语法保持）。`cargo test --workspace --all-features` 即会跑它们。

## 1. 联调工具：`peer_interop` 示例

位置：`crates/radicom-network/examples/peer_interop.rs`

功能：先做 C-ECHO；可选再做 C-STORE（发一个文件）；可选再做 C-FIND（QR）。
C-ECHO/C-STORE 返回非 0 状态时进程以非 0 退出，便于脚本判定。

```pwsh
# 仅 C-ECHO
cargo run -p radicom-network --example peer_interop -- `
  --peer 127.0.0.1:11112 --calling RADICOM --called DCM4CHE

# C-ECHO + C-STORE 一个文件 + C-FIND(STUDY)
cargo run -p radicom-network --example peer_interop -- `
  --peer 127.0.0.1:11112 --calling RADICOM --called DCM4CHE `
  --store tests-data\explicit-le\GH178.dcm `
  --find STUDY --patient-name "Doe*"
```

参数：
- `--peer host:port`（默认 `127.0.0.1:11112`）
- `--calling AE` / `--called AE`
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
$env:RADICOM_STORE_FILE = "tests-data\explicit-le\GH178.dcm"   # 可选，附带 C-STORE
cargo test -p radicom-network --test interop -- --nocapture
```

## 3. 对照 fo-dicom 的建议验收项

| 服务 | 对端（fo-dicom） | 验收标准 |
|---|---|---|
| C-ECHO | Verification SCP | 状态 0x0000 |
| C-STORE | C-STORE SCP | 状态 0x0000；对端收到的 dataset 与本端语义一致（可用 fo-dicom 端 dump 比对） |
| C-FIND | Q/R SCP | 返回匹配条数与返回键（PatientName/StudyInstanceUID 等）正确 |
| C-MOVE | Q/R SCP（+ 目的 AE） | 子操作计数 completed 正确、目的端有实体落入 |
| C-GET  | Q/R SCP | 反向 C-STORE 子操作 completed 正确、本端 reverse-store handler 收到实例 |

> C-MOVE/C-GET 需要一个"目的 AE / 反向存储"端，用 radicom 自身即可扮演
> （`collect_reverse_stores` + `DicomServer`），属于已有 loopback 覆盖范围。

## 4. 与 fo-dicom 语料的一致性（离线）

`tests-data/` 是从 fo-dicom MS-PL 测试数据导入的语料，`crates/radicom/tests/corpus.rs`
对其中 13 个样本做 **parse→write→parse 语义等价**往返；`goldens.rs`（dump/JSON/关联 PDU/
PDATA 十六进制）做逐字节/结构化对照。如需扩充，直接把 fo-dicom 生成的 `.dcm` 放入对应
`tests-data/<类别>/` 并在 `corpus.rs` 的 `CORPUS` 表登记即可。
