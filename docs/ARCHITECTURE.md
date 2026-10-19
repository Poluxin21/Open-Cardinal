# Arquitetura

O Open Cardinal segue a tríade **Kernel–Engine–Network** — "o supervisor nunca falha antes do sistema
supervisionado" — implementando o ciclo **ODA** (Observe–Decide–Act) em milissegundos.

```
   agentes (jogos, IoT, serviços)                       operadores
        │  gRPC  Pulse → Reaction                          │ CLI (TCP local + token)   HTTP (Bearer)
        ▼                                                  ▼                           ▼
┌──────────────────────────── NETWORK ───────────────────────────────────────────────────────────┐
│  grpc/        autenticação (chave de API) · validação · rate limit · tenant                    │
│  control/     plano de controle local   http/   saúde, métricas, info, auditoria               │
│  raft/        transporte entre hosts (gRPC + HMAC, mTLS opcional)                              │
└───────────────┬─────────────────────────────────────────────────────────────┬──────────────────┘
                ▼                                                             ▼
┌──────────────────────────── ENGINE ──────────────┐   ┌────────────────────── KERNEL ───────────┐
│ engine/registry   regras compiladas, hot swap    │   │ app / daemon   ciclo de vida, shutdown  │
│ engine/           leilão de prioridade           │   │ store/         redb: memória, overrides │
│ backends/         lua (legado) · wasm · ai (onnx)│◄──┤ replication    local ou Raft            │
│ engine/mem        memória compartilhada c/ quotas│   │ tenant/        registro, chaves, limites│
└──────────────────────────────────────────────────┘   │ audit/         cadeia de hash           │
                                                       │ kernel/        logs, métricas, watcher  │
                                                       └─────────────────────────────────────────┘
```

## Fluxo de um pulse

1. **Autenticação** — `authorization: Bearer <chave>` → tenant. Sem chaves emitidas, em loopback, o tenant é
   `default` (comportamento original). Emitir a primeira chave liga a autenticação.
2. **Validação** — `agent_id` ≤ 256 bytes sem caracteres de controle, telemetria limitada em entradas e bytes,
   escopo de agente da chave (`agent_prefix`), rate limit do tenant.
3. **Override do Heathcliff** — se existe um override (não expirado) para `(tenant, agent)`, ele responde e as
   regras não rodam.
4. **Regras** — as regras do agente (ou `default/`) já compiladas rodam numa thread de bloqueio, sob orçamento de
   instruções/fuel, memória e tempo. O de maior prioridade vence ([Regras](RULES.md#leilão-de-prioridade)).
5. **Resposta** + registro na auditoria (assíncrono, em lote) + métricas.

Nada aqui lê disco ou abre banco por pulse: regras ficam compiladas em memória (`ArcSwap`, troca atômica no
hot-reload) e o `redb` é aberto **uma vez** por processo.

## Layout de arquivos (`CARDINAL_HOME`, padrão: diretório atual)

```
config/config.json         configuração (criada com padrões se ausente; nunca sobrescrita)
config/tenants.json        tenants, chaves (apenas SHA-256), limites, política de IA
config/raft.json           lista de IPs do cluster (opcional → modo single-node)
config/models.json         registro de modelos ONNX (opcional)
config/admin.token         segredo do plano de controle e da API de admin (0600 / ACL só do usuário)
config/cluster.secret      segredo compartilhado entre os nós Raft (você provisiona)
config/audit.key           chave HMAC da trilha de auditoria
rules/default/*            regras de fallback do tenant "default"   (layout original)
rules/<agent_id>/*         regras de um agente                      (layout original)
tenants/<id>/rules/...     mesmo layout, para outros tenants
models/                    arquivos .onnx e tokenizers
open_cardinal.redb         memória compartilhada, overrides, log do Raft
audit.redb                 trilha de auditoria
logs/  info/               logs (modo daemon) e os JSONs legados de métricas
```

## Mapa do código

| Módulo | Responsabilidade |
|---|---|
| `config/` | Configuração tipada e validada; env overrides; caminhos; limites |
| `runtime.rs` | Detecta daemon / Docker / Swarm / Kubernetes (só muda *padrões*) |
| `store/` | Banco único; `Command` (o que é replicado); snapshot/restore; migração do formato antigo |
| `replication.rs` | `Replicator`: aplica direto (single-node) ou propõe ao Raft; `Host` das regras |
| `engine/` | Registro de regras, leilão, `SharedMem` (quotas), backends |
| `engine/backends/{lua,wasm}.rs` | Sandboxes de execução |
| `ai/` | Registro de modelos, regras `model` e `prompt` (ONNX Runtime) |
| `tenant/` | Registro, autenticação por chave, limites, rate limit |
| `grpc/` | Serviço `Sentinel` |
| `control/` | CLI ⇄ daemon (envelope com token) |
| `http/` | `/healthz` `/readyz` `/metrics` `/info` `/v1/*` |
| `audit/` | Cadeia de hash, escritor em lote, consulta, verificação |
| `raft/` | `core` (máquina de estados pura), `storage`, `transport`, `node` (driver), `discovery`, `sim` (testes) |
| `kernel/` | Logs, monitor de sistema, watcher de arquivos, sinais |
| `tls.rs` | TLS/mTLS e gerador de certificados |
| `ext.rs` | Pontos de extensão ([Edições](EDITIONS.md)) |

## Decisões de projeto

* **Determinismo.** Regras rodam em ordem de nome de arquivo; empates vão para a primeira; `math.random` do Lua é
  semeado por `(agent_id, timestamp)`; WASM não tem relógio, aleatoriedade nem WASI.
* **Falhar seguro.** Regra com erro, estouro de orçamento ou timeout → "sem opinião" (nunca derruba o daemon);
  se nenhuma regra decide, a resposta é `IDLE` (`NO_ACTION`). Sob saturação o daemon recusa rápido
  (`RESOURCE_EXHAUSTED`) em vez de enfileirar para sempre.
* **Isolamento de falha.** Cada tenant tem seu próprio limite de avaliações simultâneas; uma regra presa em código
  nativo segura *uma vaga do seu tenant*, não o daemon.
* **Núcleo do Raft puro.** `raft/core.rs` não faz I/O nem usa relógio: é dirigido por `tick()`/`step()`.
  Isso permite simular clusters inteiros (partições, perda, duplicação, crashes) e checar as invariantes de
  segurança do Raft a cada passo.
