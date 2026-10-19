# Auditoria

Toda decisão relevante, override, requisição recusada e ação administrativa vira um **registro** numa tabela
append-only (`audit.redb`). Os registros formam uma **cadeia**:

```
hash = HMAC-SHA256(config/audit.key,  hash_do_registro_anterior ‖ corpo_canônico_do_registro)
```

Alterar, remover ou reordenar qualquer registro quebra todos os hashes seguintes; sem a chave, quem edita o
arquivo não consegue recalcular uma cadeia válida. `GET /v1/audit/verify` (ou `open-cardinal audit verify`)
recalcula tudo e aponta o primeiro registro inconsistente.

## O que é registrado (`event`)

| `event` | Quando | `data` (resumo) |
|---|---|---|
| `decision` | Um pulse foi decidido | fonte (`rules`/`forced`/`idle`), ação, comando, prioridade, regra vencedora e tipo, latência, **resultado de cada regra** (inclusive erros e *evidência* de IA), telemetria |
| `decision_failed` | Erro interno ao decidir | mensagem |
| `rejected` | Autenticação falhou, pulse inválido, rate limit, token de admin errado | motivo, origem |
| `control` | Comando mutável do CLI (`force`, `tenant …`, `reload`, `stop`…) | comando (argumentos **não** são gravados: podem conter segredos) |
| `rules_reload` | Regras/tenants recarregados | contagens, problemas |
| `cluster` | Líder mudou, membership mudou, snapshot instalado, nó removido | detalhes |
| `startup` / `shutdown` | Ciclo de vida | versão, runtime, endereços |
| `audit_gap` | A fila de escrita estourou | quantos eventos foram perdidos |

Por padrão (`audit.decisions = "actions"`) só são gravadas as decisões que **fizeram algo ou deram errado**
(reação ≠ IDLE, erro de regra, override aplicado). Pulses `IDLE` rotineiros (a imensa maioria) não são gravados —
1000 agentes a 1 Hz gerariam ~1 KiB cada, 1 milhão de registros em 17 minutos. Use `"all"` se precisar provar que
*todo* pulse foi avaliado (e dimensione `retention_records`).

`audit.telemetry`: `full` (limitada a 4 KiB por registro), `keys` (só nomes) ou `none` — telemetria pode ser
sensível. Rejeições são limitadas a 50 registros/segundo (o excedente só incrementa métricas), para que um
atacante não encha a trilha.

A escrita **nunca bloqueia o pulse**: eventos entram numa fila e uma thread dedicada grava em lote (um `fsync` por
lote). Se a fila estourar, a perda é registrada na própria cadeia como `audit_gap` — lacunas são visíveis, não
silenciosas. Consultas e verificações descarregam a fila antes (você lê o que acabou de escrever).

## API HTTP

Autenticação: `Authorization: Bearer <token de admin>` (vê tudo) ou **chave de API de um tenant** (vê só o próprio
tenant; pedir outro dá `403`).

| Endpoint | |
|---|---|
| `GET /v1/audit` | Mais recentes primeiro. Filtros: `tenant`, `agent`, `event`, `trace_id`, `since`, `until` (ms Unix), `limit` (≤ 1000), `before=<seq>` (paginação; use `next_before` da resposta) |
| `GET /v1/audit/export?from=<seq>&limit=` | NDJSON, mais antigo primeiro; o cabeçalho `x-next-seq` diz de onde continuar — para alimentar SIEM/arquivamento |
| `GET /v1/audit/verify` | **admin**. `200` com `{"ok":true,"checked":N,…}` ou `409` com `broken_at` e `reason` |

Exemplo de registro:

```json
{ "seq": 4812, "ts_ms": 1791050839042, "node": "c6b31bfe", "tenant": "acme", "agent": "eu-pump-7",
  "event": "decision", "trace_id": "9f2c1d0a7b3e44aa",
  "data": { "source": "rules", "action": 1, "command": "AI_CUTOFF", "priority": 400,
            "winner": "overheat", "winner_kind": "prompt", "latency_us": 38211,
            "rules": [ { "rule": "guard", "kind": "wasm", "result": "none", "us": 31 },
                       { "rule": "overheat", "kind": "prompt", "result": "output",
                         "detail": { "action": "SHUTDOWN", "priority": 400,
                                     "evidence": "model=qwen-small sha256=… prompt_sha256=… probs=[ok:0.02 shutdown:0.97]" } } ],
            "telemetry": { "engine_temp": "96", "vibration": "3" } },
  "prev": "…64 hex…", "hash": "…64 hex…" }
```

O `trace_id` é o mesmo devolvido ao agente em `Reaction.trace_id`, então um desligamento no campo é rastreável
até a regra, o modelo e a entrada que o causaram.

## Retenção, chave e cluster

* `audit.retention_records` poda os mais antigos (a verificação parte do primeiro registro restante, cujo `prev`
  é confiável por definição).
* `config/audit.key` (0600 / ACL só do usuário) autentica a cadeia; sem ela, `verify` recusa ("a chave não
  corresponde"). **Faça backup da chave.** Mantenha-a fora do alcance de quem pode editar `audit.redb`.
* Cada nó mantém a **sua** cadeia (o campo `node` identifica a origem). Para uma visão central, exporte por
  `/v1/audit/export` ou use um *sink* ([Edições](EDITIONS.md#pontos-de-extensão)).

## Métricas (Prometheus)

`GET /metrics?format=prometheus` (ou o `Accept` padrão do Prometheus):

```
cardinal_pulses_total · cardinal_pulses_rejected_total · cardinal_forced_reactions_total
cardinal_rule_errors_total · cardinal_in_flight_pulses
cardinal_reactions_total{action="idle|shutdown|restart|custom"}
cardinal_tenant_pulses_total{tenant="…"} · cardinal_tenant_rejected_total{tenant="…"}
cardinal_decision_latency_us_bucket{le="…"} (+ _sum, _count)
cardinal_rules_loaded · cardinal_rule_load_issues · cardinal_uptime_seconds
cardinal_cpu_usage_percent · cardinal_memory_used_kib
cardinal_audit_records · cardinal_audit_dropped_total
cardinal_raft_{term,is_leader,has_leader,commit_index,applied_index,last_index,voters,learners}
```

`/metrics` sem parâmetros continua devolvendo o JSON original `{"total_rules","agents_detected","connected_agents"}`
(`connected_agents` agora é o número de pulses **em andamento** — antes vazava e crescia para sempre).

```yaml
# prometheus.yml
scrape_configs:
  - job_name: cardinal
    params: { format: [prometheus] }
    authorization: { credentials_file: /etc/prometheus/cardinal.token }   # quando a autenticação está ligada
    static_configs: [ { targets: ["cardinal-0:8080", "cardinal-1:8080", "cardinal-2:8080"] } ]
```
