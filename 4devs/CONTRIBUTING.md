# Open Cardinal

## Architecture & Development Guide

---

## 1. Project Definition

**Open Cardinal** is a high-performance, low-resource **Autonomous Supervisor (Daemon)**. It operates under the **Sidecar architecture pattern**, running alongside a Target System to monitor telemetry, detect anomalies, and execute automated corrections in real time.

Unlike passive monitoring systems (such as Prometheus), Cardinal possesses **agency**: it has permission to intervene, alter states, and restart subsystems based on programmable logic.

### Core Philosophy: The ODA Cycle

The system operates in an infinite, blocking loop composed of three stages:

* **Observe**
  Telemetry ingestion via **gRPC / UDS**.

* **Decide**
  Data evaluation against **hot-swappable Lua scripts**.

* **Act**
  Dispatching correction commands or alerts.

---

## 2. Tech Stack

The project follows a **Rust-First philosophy**, ensuring memory safety, latency predictability, and robustness in low-infrastructure environments.

* **Language:** Rust (Edition 2021+)
* **Async Runtime:** Tokio (Non-blocking I/O and multithreading)
* **Communication:** Tonic (gRPC) + Prost (Protobuf)
* **Scripting Engine:** LuaJIT (via `mlua` crate) – dynamic business logic

### Database (Embedded)

* **Sled** – Pure Rust key-value store
* **or SQLite** – WAL mode

### Serialization

* **Bincode** – internal / disk
* **Protobuf** – external / network

---

## 3. Development Guide

### Prerequisites

#### Rust Toolchain

```bash
rustup install stable
```

#### Protocol Buffers Compiler

**Linux**

```bash
sudo apt install protobuf-compiler
```

**macOS**

```bash
brew install protobuf
```

#### LuaJIT (Optional)

Used for local script testing.

---

### Build & Execution

The system aims to be **statically compiled** to facilitate distribution in low-infrastructure environments.

```bash
# 1. Generate Rust code from .proto files
cargo build --package cardinal-proto

# 2. Run the Daemon in development mode (verbose logs)
RUST_LOG=debug cargo run --bin cardinal-d -- --config dev_config.toml

# 3. Run the test suite (unit + integration)
cargo test
```

---

## Code Standards

* **No `unwrap()` in production**
  Use `match` or `?`. The daemon must never panic.

* **Zero-Copy whenever possible**
  Prefer passing references (`&[u8]`, `Bytes`) instead of cloning `String` or `Vec<u8>`.

* **Async Traits**
  Use the `async-trait` crate to define interfaces between modules.

---

## Debugging Tools

* **Tokio Console**
  Monitor stuck tasks, starvation, and potential deadlocks.

* **Flamegraph**
  CPU profiling for high-load scenarios.

---

## Closing Notes

Open Cardinal is designed to be **deterministic**, **auditable**, and **autonomous** — a true supervisor for modern systems.
