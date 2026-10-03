# Configuração

Tudo tem padrão seguro; um diretório vazio já sobe um daemon funcional. O arquivo `config/config.json` é criado
(com os padrões) se não existir ou estiver em branco, e **nunca é sobrescrito**. O formato original
(`{"grpc_port":50051,"http_port":8080,"db_file":"open_cardinal.redb"}`) continua válido.
Chaves desconhecidas são **rejeitadas** (um erro de digitação em uma opção de segurança não pode passar
despercebido).

## `config/config.json`

| Chave | Padrão | Descrição |
|---|---|---|
| `grpc_port` / `http_port` | `50051` / `8080` | Portas dos agentes e do HTTP |
| `db_file` | `open_cardinal.redb` | Banco principal (relativo ao home) |
| `db_cache_mb` | `32` | Cache de páginas do redb (o padrão do redb é 1 GiB) |
| `bind` | `"auto"` | `"auto"` (loopback no daemon, todas as interfaces em containers), `"loopback"`, `"all"` ou lista de IPs `["10.0.0.5"]` |
| `control_addr` | `127.0.0.1:19876` | Plano de controle do CLI. **Precisa ser loopback** |
| `security.mode` | `"auto"` | `auto`: aberto até existir uma chave de API (em container/exposto: exige credenciais); `open`; `required` |
| `security.allow_insecure_remote` | `false` | Permite servir sem autenticação fora do loopback (**só laboratório**) |
| `security.tls` | — | `{cert_file, key_file, client_ca_file?}`; com `client_ca_file` vira **mTLS**. Vale para gRPC dos agentes e HTTP |
| `limits.*` | ver abaixo | Limites globais (cada tenant pode sobrescrever) |
| `log.output` | `"auto"` | `file` (daemon), `stdout` (containers), `both` |
| `log.format` / `log.level` | `text` / `info` | `json` para ingestão; `level` aceita filtros do `tracing` |
| `audit.enabled` | `true` | |
| `audit.decisions` | `"actions"` | `actions`: ações ≠ IDLE, erros de regra, overrides. `all`: tudo (~1 KiB por pulse) |
| `audit.retention_records` | `500000` | Registros mais antigos são podados (0 = nunca) |
| `audit.telemetry` | `"full"` | `full` / `keys` (só nomes) / `none` |
| `export_info_files` | `true` | Continua escrevendo `info/sys.json` e `info/metrics.json` (integrações legadas) |

### Limites (`limits`, também por tenant)

| Chave | Padrão | |
|---|---|---|
| `max_telemetry_entries` / `max_telemetry_bytes` | 256 / 65536 | Tamanho do pulse |
| `rule_timeout_ms` | 250 | Orçamento de relógio para todas as regras de um pulse |
| `lua_instructions` / `lua_memory_bytes` | 5 000 000 / 16 MiB | Por regra Lua |
| `wasm_fuel` / `wasm_memory_bytes` | 20 000 000 / 16 MiB | Por regra WASM |
| `ai_timeout_ms` | 2000 | Orçamento das regras de IA |
| `max_concurrent_evaluations` | 0 (= 2 × núcleos) | Avaliações simultâneas |
| `rate_limit_per_sec` / `rate_limit_burst` | 0 (ilimitado) | Token bucket |
| `kv_max_entries` / `kv_max_writes_per_eval` | 100000 / 64 | Cota de memória compartilhada |

## Variáveis de ambiente

| Variável | Efeito |
|---|---|
| `CARDINAL_HOME` | Diretório de trabalho (`--home`) |
| `CARDINAL_RUNTIME` | Força `daemon` \| `docker` \| `swarm` \| `kubernetes` |
| `CARDINAL_SWARM_SERVICE` | `{{.Service.Name}}` no stack do Swarm: marca o runtime Swarm |
| `CARDINAL_GRPC_PORT` `CARDINAL_HTTP_PORT` `CARDINAL_DB_FILE` `CARDINAL_CONTROL_ADDR` | Sobrescrevem o `config.json` |
| `CARDINAL_BIND` | `auto` \| `loopback` \| `all` \| `ip1,ip2` |
| `CARDINAL_SECURITY` | `auto` \| `open` \| `required` |
| `CARDINAL_LOG` `CARDINAL_LOG_OUTPUT` `CARDINAL_LOG_FORMAT` | Logs (`RUST_LOG` também funciona) |
| `CARDINAL_AUDIT` | `false` desliga a auditoria |
| `CARDINAL_ADMIN_TOKEN` / `_FILE` | Token de admin (segredos de container) |
| `CARDINAL_TOKEN` | Token usado pelo CLI |
| `CARDINAL_RAFT_PEERS` `CARDINAL_RAFT_PORT` `CARDINAL_RAFT_SELF_IP` `CARDINAL_RAFT_CLUSTER_ID` | Cluster sem `raft.json` |
| `CARDINAL_CLUSTER_SECRET` / `_FILE` | Segredo compartilhado do cluster |
| `CARDINAL_API_KEY` | Chave de API do cliente de teste (`client`) |

## Padrões por runtime

| | daemon | Docker / Swarm / Kubernetes |
|---|---|---|
| `bind` | loopback (`::1` e `127.0.0.1`) | `0.0.0.0` |
| Autenticação | aberta até a 1ª chave | **obrigatória** desde o início |
| Logs | `logs/cardinal_log.<data>` | stdout |
| Parada | SIGINT/SIGTERM/Ctrl-C ou `open-cardinal stop` | SIGTERM (líder transfere a liderança antes) |

## CLI

```
open-cardinal                              # sem subcomando: roda o daemon
open-cardinal status | stats | reload | stop | health
open-cardinal heathcliff force --agent A --force 0|1|2|3 [--cmd NOME] [--param k=v]... [--ttl SEG] [--tenant T]
open-cardinal heathcliff revoke_force --agent A | list
open-cardinal tenant list | add <id> | issue-key <id> [--label L] [--agent-prefix P] | revoke-key <id> <label> | enable|disable <id>
open-cardinal rules list [--tenant T] | issues
open-cardinal audit verify | tail [--limit N] [--tenant T] [--agent A] [--event E]
open-cardinal raft status | members | add-peer <ip> | remove-peer <ip> | transfer-leader [ip] | snapshot | keygen
open-cardinal tls init-ca --dir certs | issue --dir certs --name node1 --ip 10.0.0.11 [--dns host]
```

Opções globais: `--home <dir>`, `--token <t>`. `heathcliff` aceita a sintaxe da wiki (`-- --agent X`) e também sem `--`.

## Endpoints HTTP

| Endpoint | Autenticação | |
|---|---|---|
| `GET /healthz` | nunca | liveness |
| `GET /readyz` | nunca | readiness (regras carregadas, banco ok, e — em cluster — membro com líder conhecido) |
| `GET /metrics` | só se a autenticação estiver ligada | JSON original; `?format=prometheus` ou `Accept` do Prometheus → texto Prometheus |
| `GET /info` | idem | JSON original (kernel, CPU, memória) |
| `GET /v1/status` `/v1/rules` `/v1/tenants` | admin ou chave de tenant (visão do próprio tenant) | |
| `GET /v1/cluster` | admin | estado do Raft |
| `GET /v1/audit` `/v1/audit/export` | admin ou chave de tenant (somente o próprio tenant) | [Auditoria](AUDIT.md) |
| `GET /v1/audit/verify` | admin | verifica a cadeia |

Autenticação: `Authorization: Bearer <token de admin | chave de API>`.
