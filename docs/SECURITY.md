# Segurança

## Metodologia

A refatoração começou **executando o programa original** (build do `HEAD`, daemon + cliente + CLI + HTTP) e só
então procurando falhas. O critério de prioridade foi o pedido: **uma vulnerabilidade só vira prioridade com um
PoC reproduzível e impacto alto ou crítico.** Cada PoC abaixo foi executado contra o binário original e
**reexecutado contra o novo**; hoje todos são **testes automatizados de regressão** (`tests/single_node.rs`).

| ID | Achado | Severidade | PoC | Corrigido em | Teste de regressão |
|---|---|---|---|---|---|
| **F-01** | Plano de controle (`127.0.0.1:19876`) sem autenticação: qualquer processo local para o daemon e força `SHUTDOWN` em qualquer agente | **Alta** | PoC 2 | token de admin, envelope autenticado | `poc2_control_plane_refuses_unauthenticated_commands` |
| **F-02** | gRPC sem autenticação: quem alcança a porta forja o `agent_id` de outro, **apaga/envenena o estado dele e suprime um SHUTDOWN** | **Alta** | PoC 1 | chaves de API por tenant, escopo por agente | `poc1_agent_spoofing_is_blocked_once_keys_exist` |
| **F-03** | Regras Lua com a stdlib completa: `os.execute`/`io` rodavam como o usuário do daemon — **ao salvar o arquivo**, sem pulse algum | **Alta** (crítica com multi-tenant) | PoC 3 | sandbox, regras só compiladas ao carregar | `poc3_rules_cannot_run_operating_system_commands` |
| **F-04** | `while true do end` numa regra trava o daemon inteiro (22 pulses bastam; CPU 100 %; gRPC, HTTP e controle mudos) | **Alta** | PoC 4 | orçamentos + execução fora do runtime async | `poc4_an_infinite_loop_rule_cannot_freeze_the_daemon` |
| F-05 | `Database already open` sob concorrência: com 4+ conexões 84–94 % dos pulses falhavam e `connected_agents` vazava | Alta (confiabilidade) | bench | banco aberto uma única vez; contador RAII | `concurrent_pulses_never_fail_and_the_counter_does_not_leak` |
| F-06 | `redb_api.get` de chave ausente dava *panic*: o exemplo de persistência da própria wiki derrubava o worker a cada pulse | Média | teste | `get` devolve `nil` | `the_wiki_persistence_example_works_on_the_very_first_run` |
| F-07 | HTTP em `0.0.0.0:8080` sem autenticação revelava versão do kernel, uso de CPU/memória e contagens | Baixa/Média | `curl` | bind em loopback por padrão; autenticação exigida quando exposto | `secure_by_default_in_containers_and_when_exposed` |
| F-08 | *Log forging*: `agent_id` com `\n` injetava linhas falsas no log (confirmado: surgiu `INFO FAKE: operator admin logged in from 10.0.0.1`) | Média | `client ping --id $'x\n<linha falsa>'` | validação + escape em todo log de dado externo | `invalid_pulses_are_rejected_not_processed` |
| F-09 | Dependências com avisos: `h2 0.4.13` (RUSTSEC-2026-0258), `anyhow`, `crossbeam-epoch`, `rand 0.8.5` | Média | OSV | atualizadas; `memmap2` (sem uso) removida | CI: `cargo audit` |
| F-10 | `Stop` encerrava com `process::exit` no meio da requisição (confirmado: no Windows o CLI recebia `ConnectionReset`); `config.json` era sobrescrito a cada início; `lua_check` marcava regras válidas como erro | Baixa | — | parada graciosa; config preservada | `graceful_stop_via_cli_answers_then_exits`, `the_original_three_key_config_file_is_honoured_not_overwritten` |

Não reproduzi como explorável a travessia de diretório em `find_rule_by_agent` (a higienização original era
frágil, mas não achei uma entrada que escapasse). Ainda assim ela foi substituída: o `agent_id` agora só é comparado
**por igualdade** com nomes de pasta já validados (`[A-Za-z0-9._-]`, sem symlinks) — nunca vira caminho.

## Reproduzindo os PoCs

Contra o binário **original** (`git archive <commit antigo>`), com um cliente que envie telemetria arbitrária
(hoje: `client ping --id <agente> -t chave=valor`):

**PoC 1 — falsificação de agente** (regra de strikes da wiki em `rules/Victim/`):

```bash
client ping --id Victim -t cpu_temp=95      # dispositivo real: strike 1
client ping --id Victim -t cpu_temp=95      # dispositivo real: strike 2
client ping --id Victim -t cpu_temp=20      # ATACANTE, sem credenciais, como "Victim": zera o contador
client ping --id Victim -t cpu_temp=95      # 3ª leitura quente do real → esperado SHUTDOWN; recebeu IDLE
```
Original: o SHUTDOWN é **suprimido**. Novo: o atacante recebe `Unauthenticated` e o dispositivo real é desligado.

**PoC 2 — plano de controle**:

```python
import socket, json
s = socket.create_connection(("127.0.0.1", 19876)); s.sendall(json.dumps(
  {"Heathcliff": {"command": "force", "args": ["--agent", "Healthy_Pump", "--force", "1"]}}).encode())
s.shutdown(socket.SHUT_WR); print(s.recv(4096))      # original: {"Ok":{"message":"Agent Healthy_Pump"}}
# e `"Stop"` encerra o daemon
```
Novo: `{"Error":{"message":"unauthenticated or malformed request…"}}`; sem o token de `config/admin.token` nada
acontece, e `Stop` sem credencial não para o daemon.

**PoC 3 — execução de comandos**: salve `rules/Pwn/x.lua` com `os.execute("echo pwned > pwned.txt")`.
Original: `pwned.txt` aparece em ~1 s, sem nenhum pulse. Novo: `os` não existe; o erro aparece na auditoria.

**PoC 4 — negação de serviço**: `rules/Loop/loop.lua` = `while true do end` e ~22 pulses para o agente `Loop`.
Original: 13 dos 16 núcleos a 100 % e, com mais alguns pulses, **controle, HTTP e gRPC sem resposta**. Novo: 200
pulses simultâneos terminam com erro de orçamento por regra; o daemon segue respondendo.

## O que foi feito

* **Autenticação.** Agentes: chave de API (hash SHA-256 armazenado, comparação por hash), escopo por prefixo de
  agente, tenant desabilitável. Operadores: token de admin (`config/admin.token`, 0600 / ACL do usuário atual em
  Windows, comparação em tempo constante). Emitir a primeira chave **liga** a autenticação no modo `auto`.
* **Padrões seguros.** Daemon: loopback. Container ou bind exposto: autenticação obrigatória. Servir sem
  autenticação fora do loopback é **recusado na inicialização** (`security.allow_insecure_remote` existe, com esse
  nome, para o laboratório).
* **TLS/mTLS** no gRPC dos agentes, no HTTP e entre nós Raft; gerador de certificados embutido.
* **Sandbox de regras.** Lua sem `os`/`io`/…; WASM sem WASI, só a API `cardinal`; orçamentos de instruções/fuel,
  memória e relógio; avaliação em threads de bloqueio com *gates* por tenant; falha de uma regra não afeta as
  outras; *panic* num backend é contido.
* **Validação de entrada** em todo pulse (tamanhos, caracteres de controle) e em todo arquivo (tamanho de
  regras/manifestos, nomes de pasta, symlinks ignorados, modelos confinados a `models/`).
* **Limites e recusa rápida.** Rate limit por tenant, gates de concorrência, cotas de memória compartilhada;
  saturação → `RESOURCE_EXHAUSTED`.
* **Auditoria à prova de adulteração** ([Auditoria](AUDIT.md)) e logs com dados externos escapados.
* **Cluster.** HMAC com janela de 60 s, mTLS opcional, entrada somente com o próprio IP, segredo obrigatório.
* **Plano de controle:** limite de tamanho (64 KiB) e de tempo, ≤ 32 conexões, loopback obrigatório, auditoria de
  comandos mutáveis.

## Modelo de ameaças (resumo)

| Quem | Confiamos? | Defesa |
|---|---|---|
| Operador com acesso ao diretório do daemon | Sim (é root efetivo do serviço) | permissões de arquivo |
| Processo local não privilegiado | **Não** | token de admin; chave de API; loopback |
| Agente (rede) | Não | chave de API + escopo + validação + rate limit |
| Autor de regra de um tenant | Não (WASM) / parcialmente (Lua) | sandbox + orçamentos + gates |
| Outro nó da rede tentando entrar no cluster | Não | segredo HMAC, mTLS |
| Telemetria como vetor de *prompt injection* | Não | só *tokens* curtos entram no prompt; IA com cardápio fechado e teto de prioridade |

## Hardening recomendado

1. Emita chaves por frota (`--agent-prefix`) e rotacione-as (`issue-key` + `revoke-key`).
2. Ligue **mTLS** (`open-cardinal tls init-ca`, `tls issue`) em qualquer exposição além do host.
3. Restrinja a porta do cluster por firewall/NetworkPolicy; use segredos de container (`*_FILE`).
4. Hostis ⇒ **WASM** (`rules.lua: false`, o padrão para tenants novos); mantenha `ai.max_priority` baixo.
5. `audit.enabled`, backup de `config/audit.key`, exporte para o SIEM.
6. Monitore `cardinal_pulses_rejected_total`, `cardinal_audit_dropped_total`, `cardinal_rule_load_issues`.
7. Rode `cargo audit` no CI (já configurado em `.github/workflows/ci.yml`).

## Riscos residuais (honestos)

* **Lua**: uma única chamada nativa (ex.: casamento de padrões catastrófico) pode superar o gancho de instruções e
  ocupar *uma vaga* do tenant por mais que o orçamento (o daemon continua de pé; o tenant fica limitado a menos
  vagas). Por isso Lua vem **desligado** para tenants novos.
* **Modo aberto em loopback** (daemon sem chaves) confia em qualquer processo local, como o original. Emita uma
  chave para fechar.
* O token de admin e as chaves trafegam em texto no loopback (CLI ⇄ daemon) e em HTTP sem TLS: ligue TLS se isso
  sair do host.
* **Rotação**: segredo do cluster, certificados e token exigem reinício (não há recarga a quente).
* **Relógios**: o HMAC do cluster tolera ±60 s de diferença.
* **HTTP**: não há *timeout* de leitura de cabeçalhos (Slowloris) no servidor HTTP embutido; use um proxy se a porta
  for pública.
* **IA**: é estatística. As defesas (cardápio, teto de prioridade, auditoria, sanitização) limitam o dano, não
  tornam o modelo infalível.
* Cadeia de auditoria: protege contra adulteração por quem **não** tem `audit.key`.
* Dependência `paste` (transitiva) está sem manutenção (aviso informativo, sem vulnerabilidade).
