# O que é o Open Cardinal?

<!--  -->

**Open Cardinal** é um daemon sidecar determinístico de alta performance, projetado para monitoramento de infraestrutura crítica e tomada de decisão automatizada.

Ele atua como o **"sistema imunológico"** das suas aplicações, conectando diversos agentes (Servidores de Jogos, IoT, Microserviços) a um motor lógico **Lua hot-swappable** via **gRPC**.

---

## Documentação

A documentação completa foi movida para a **Wiki do Projeto**.

* **Arquitetura**: Entenda a tríade **Kernel–Engine–Network**.
* **Referência da API Lua**: Como escrever regras para os seus agentes.
* **Protocolo gRPC**: As definições de contrato **Pulse** e **Reaction**.

---

## Quick Start

Coloque o Open Cardinal para rodar em menos de **2 minutos**.

### 1. Instalação

```bash
# Clone o repositório
git clone https://github.com/Poluxin21/open-cardinal.git
cd open-cardinal

# Compile o binário de release
cargo build --release
```

### 2. Rodar o Daemon

```bash
# Inicia o servidor (Escuta na porta 50051)
./target/release/open-cardinal
```

### 3. Simular um Agente

Abra um novo terminal e use a CLI integrada para testar as regras:

```bash
# Simula um foguete enviando telemetria a cada 500ms
cargo run --bin client -- simulate --id "Rocket_01" --interval 500
```

---

## Stack Tecnológica

* **Core**: Rust (Tokio, Tonic, Prost)
* **Scripting**: Lua 5.5 (mlua)
* **Observabilidade**: OpenTelemetry & Tracing

---

## Contribuindo

Contribuições são bem-vindas! Por favor, leia nosso **Guia de Contribuição** e verifique a aba **Issues**.

---

## Licença

Distribuído sob a **Licença MIT**.
