# O que é o Open Cardinal?

**Open Cardinal** é um daemon sidecar determinístico de alta performance, projetado para monitoramento de infraestrutura crítica e tomada de decisão automatizada.

Ele atua como o **"sistema imunológico"** das suas aplicações, conectando diversos agentes (Servidores de Jogos, IoT, Microserviços) a um motor de regras **hot-swappable** via **gRPC**. As regras podem ser escritas em **WebAssembly**, em **Lua** (modo legado), ou delegadas a um **modelo de IA** (ONNX) — sempre com limites de execução, isolamento por tenant e trilha de auditoria à prova de adulteração.

Devlog / bastidores: https://whatsapp.com/channel/0029VbCnH7b5a23vwR6biY40

---

## Documentação

| Documento | Conteúdo |
|---|---|
| [Arquitetura](docs/ARCHITECTURE.md) | Kernel–Engine–Network, módulos, fluxo de um pulse, layout de arquivos |
| [Configuração](docs/CONFIGURATION.md) | `config.json`, variáveis de ambiente, CLI, endpoints HTTP |
| [Regras](docs/RULES.md) | Lua (legado), **WebAssembly**, **IA/ONNX**, leilão de prioridade |
| [Multi-tenant](docs/MULTITENANCY.md) | Tenants, chaves de API, limites, isolamento |
| [Cluster (Raft)](docs/CLUSTER.md) | Memória compartilhada entre hosts; daemon, Docker, Swarm, Kubernetes |
| [Auditoria](docs/AUDIT.md) | Cadeia de hash, API `/v1/audit`, métricas Prometheus |
| [Segurança](docs/SECURITY.md) | Vulnerabilidades corrigidas (com PoCs), modelo de ameaças, hardening |
| [Edições](docs/EDITIONS.md) | Open core × Enterprise/SaaS, pontos de extensão |
| [Migração](docs/MIGRATION.md) | O que mudou em relação à versão anterior |

A wiki original continua válida para o contrato gRPC (`Pulse` / `Reaction`) e para o módulo Heathcliff.

---

## Quick Start

### 1. Instalação

```bash
git clone https://github.com/Poluxin21/open-cardinal.git
cd open-cardinal
cargo build --release          # não precisa instalar o protoc
```

### 2. Rodar o daemon

```bash
./target/release/open-cardinal      # gRPC :50051 · HTTP :8080 · só em loopback por padrão
```

Na primeira execução ele cria `config/`, `rules/default/`, o banco `open_cardinal.redb` e um token de
administração em `config/admin.token`.

### 3. Simular um agente

```bash
# um pulse com telemetria própria
cargo run --release --bin client -- ping --id Rocket_01 -t fuel=53

# um foguete enviando telemetria a cada 500ms
cargo run --release --bin client -- simulate --id Rocket_01 --interval 500

# teste de carga: 16 agentes, 20 mil pulses
cargo run --release --bin client -- bench -c 16 -r 20000 --connection-per-worker
```

### 4. Operar

```bash
open-cardinal status                                   # estado do daemon
open-cardinal heathcliff force --agent Rocket_01 --force 1   # override manual (SHUTDOWN)
open-cardinal heathcliff revoke_force --agent Rocket_01
open-cardinal rules list                               # regras carregadas
open-cardinal audit tail --limit 5                     # últimas decisões
curl localhost:8080/metrics                            # JSON (formato original)
curl 'localhost:8080/metrics?format=prometheus'        # Prometheus
```

Os comandos do CLI leem o token de `config/admin.token`: rode-os no diretório do daemon ou passe `--home <dir>` /
`--token`.

---

## O que há de novo

* **Raft** — vários hosts compartilham memória (`redb_api`, overrides do Heathcliff). Você informa **só os IPs**
  em `config/raft.json`; porta, identidade, líder e membros são descobertos. Funciona como daemon, Docker,
  Docker Swarm e Kubernetes ([Cluster](docs/CLUSTER.md)).
* **WebAssembly** — regras em qualquer linguagem que compile para WASM, medidas por *fuel* e memória, sem
  acesso a arquivos/rede/relógio ([Regras](docs/RULES.md#webassembly)).
* **IA embarcada (ONNX Runtime)** — modelos como regras ou regras escritas em linguagem natural, com limites
  (a IA nunca supera uma regra determinística) e evidências na auditoria
  ([Regras](docs/RULES.md#ia-onnx)). Opcional: `cargo build --features prompt`.
* **Multi-tenant** — regras, memória, overrides, limites e chaves isolados por tenant
  ([Multi-tenant](docs/MULTITENANCY.md)).
* **Auditoria** — cadeia de hash HMAC, consulta/exportação por HTTP, verificação de integridade
  ([Auditoria](docs/AUDIT.md)).
* **Segurança** — plano de controle autenticado, autenticação de agentes por chave de API, TLS/mTLS, sandbox de
  regras, limites de CPU/memória/tempo ([Segurança](docs/SECURITY.md)).
* **Desempenho** — de ~50 para até ~**37.000 pulses/s**, p50 de 19 ms para **0,23 ms**, e zero falhas sob
  concorrência (antes 84–94 % dos pulses falhavam com 4+ conexões). Detalhes em [Migração](docs/MIGRATION.md).

## Containers

```bash
docker build -t open-cardinal .                       # imagem distroless, usuário não-root
docker build -t open-cardinal:ai --build-arg FEATURES=prompt .   # com ONNX Runtime
docker compose -f deploy/docker/docker-compose.yml up -d         # cluster de 3 nós
```

Manifestos: [`deploy/docker`](deploy/docker), [`deploy/swarm`](deploy/swarm), [`deploy/kubernetes`](deploy/kubernetes).

## Features de compilação

| Feature | Padrão | O que habilita |
|---|---|---|
| `lua` | sim | regras Lua (legado, sandboxed) |
| `wasm` | sim | regras WebAssembly (wasmi) |
| `tls` | sim | TLS/mTLS e o gerador `open-cardinal tls` |
| `onnx` | não | ONNX Runtime embarcado, regras de modelo |
| `prompt` | não | `onnx` + tokenizer: regras em linguagem natural |

---

## Contribuindo

Contribuições são bem-vindas! Por favor, leia nosso **Guia de Contribuição** e verifique a aba **Issues**.

```bash
cargo test                 # unitários + integração com processos reais (inclui cluster Raft e TLS)
cargo test --features prompt ai::
RAFT_CHAOS_SEEDS=3000 cargo test --release raft::sim::tests::chaos   # soak do Raft
```

---

## Licença

Distribuído sob a **Licença MIT**.
