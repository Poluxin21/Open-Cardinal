# Migração da versão anterior (0.1)

A refatoração preserva o produto: o contrato gRPC (`Pulse`/`Reaction`), o formato das regras (a tabela
`action/cmd_name/priority/params`), o leilão de prioridade, o Heathcliff, `redb_api`, `/metrics` e `/info`,
o layout `rules/…` e os comandos do CLI continuam como estavam. Abaixo, tudo que **mudou de comportamento** —
na ordem em que pode afetar você.

## Mudanças que podem exigir ação

| O que mudou | Por quê | O que fazer |
|---|---|---|
| **O CLI precisa do token de admin.** Ele lê `config/admin.token` do diretório atual (ou `--home`, `--token`, `CARDINAL_TOKEN`) | O plano de controle aceitava comandos de qualquer processo local (F-01) | Rode o CLI no diretório do daemon, ou passe `--home <dir>` |
| **gRPC e HTTP escutam em loopback por padrão** (antes: gRPC em `[::1]`, HTTP em `0.0.0.0`) | Exposição involuntária (F-07) | Para expor: `"bind": "all"` ou IPs — **exige autenticação** (recusado na inicialização sem ela) |
| **Emitir uma chave de API liga a autenticação** dos agentes | Fechar o F-02 sem configuração extra | Passe `authorization: Bearer ck_…` nos agentes (`--api-key` no cliente de teste) |
| **Containers** (Docker/Swarm/Kubernetes) exigem autenticação desde o início e escutam em `0.0.0.0` | Loopback dentro de um container é compartilhado com outros containers do pod | `open-cardinal tenant issue-key default` |
| **Regras Lua têm sandbox**: sem `os`, `io`, `package`, `debug`, `coroutine`, `dofile`, `loadfile`, `load`, `require`, `collectgarbage`, `print`; com limites de instruções/memória/tempo | F-03/F-04 | Use `cardinal.log(msg)` no lugar de `print`; regras que dependiam de `os.time()`/`io` precisam de outra fonte (`pulse.timestamp`) |
| `redb_api.get` devolve **`nil`** para chave ausente (antes: *panic*) | F-06 | Nada (o exemplo da wiki passa a funcionar) |
| **Ação desconhecida** numa regra (`"SHUTDWN"`) é erro e a regra é ignorada (antes: virava `IDLE` e podia vencer o leilão) | Falhar alto | Corrija o texto da ação |
| `heathcliff force` exige `--agent` e `--force` válidos (antes: respondia "ok" sem fazer nada, e usava o agente `default` se faltasse `--agent`) | Um override de segurança nunca pode falhar em silêncio | Informe os dois |
| **Nomes de pasta de agente** devem casar exatamente (maiúsculas/minúsculas) e usar só `[A-Za-z0-9._-]` | Comportamento igual em todos os SOs; sem travessia | Renomeie pastas incomuns |
| `Reaction.trace_id` é um **identificador único por pulse** (antes: `"idle"`, `"multi-script"` ou o `agent_id`) | Correlação com a auditoria e com tracing distribuído | Se algum cliente comparava com `"idle"`, use `type == IDLE` |
| Ordem das regras: **por nome de arquivo** (antes: ordem do sistema de arquivos, diferente entre SOs) | Determinismo | Se dependia da ordem, prefixe os nomes (`10_guard.lua`) |
| `config.json` **não é mais sobrescrito** a cada início, e chaves desconhecidas são rejeitadas | Sua configuração era descartada; typos passavam | Remova chaves inválidas |
| `queue.redb` **não é mais usado**; tudo vive em `open_cardinal.redb` (a tabela antiga de `redb_api` é migrada para o tenant `default` no primeiro início) | Um único handle de banco | Overrides do Heathcliff ativos precisam ser reaplicados (são efêmeros por natureza) |
| `connected_agents` em `/metrics` = pulses **em andamento** (antes vazava e só crescia) | Bug | — |
| A regra `default.lua` agora é compilada ao carregar, não executada: o ERROR "attempt to index a nil value (global 'pulse')" no log desapareceu | Bug | — |

## O que é novo e opcional

* `config/raft.json` (cluster), `config/tenants.json` (multi-tenant), `config/models.json` (IA), `security.tls`.
* Regras WASM, regras de IA, `redb_api.incr/del`, `cardinal.log`.
* `open-cardinal tenant|rules|audit|raft|tls|health` e os endpoints `/v1/*`, `/healthz`, `/readyz`.
* Features de compilação `onnx` e `prompt` (o build padrão não baixa o ONNX Runtime).

## Dependências

* Removidas por não serem usadas: `protobuf`, `uguid` e `memmap2` (esta tinha um aviso de segurança). `rand` agora
  também gera tokens e chaves (CSPRNG do SO).
* Novas (por finalidade): `wasmi` (WASM), `sha2`/`hmac`/`subtle` (chaves e auditoria), `arc-swap` (troca atômica de
  regras), `thiserror`, `if-addrs` (descoberta de IP), `rustls`/`axum-server`/`rcgen` (feature `tls`),
  `ort`/`tokenizers` (features `onnx`/`prompt`).
* `protoc` **não precisa mais ser instalado** (`protoc-bin-vendored`): o CI não instala mais nada. Se `PROTOC`
  estiver definido, ele é respeitado.

## Medidas (release, mesma máquina, regra padrão, agentes distintos)

| | Antes | Depois |
|---|---|---|
| 1 conexão, sequencial | 52 req/s, p50 19 ms | **~4 300 req/s, p50 0,23 ms** |
| 4 conexões | **84 % de erros** | 0 erros (13 000 req/s, p99 0,5 ms) |
| 16 conexões | **94 % de erros** | 0 erros (30 000 req/s, p99 1,3 ms) |
| 64 conexões | — | 0 erros (37 000 req/s, p99 4,5 ms) |
| Memória em repouso | 23 MB | 26 MB |
| Memória após 300 mil pulses | — | 30 MB (auditoria em `actions`) / 62 MB (`all`) |
| Falhas sob concorrência / vazamento de `connected_agents` | sim / sim | não / não |

(Cada conexão separada = como agentes reais; multiplexar todos numa única conexão HTTP/2 no cliente de bench
mostra p99 ≈ 20 ms no Windows por artefato do cliente, não do daemon.)
