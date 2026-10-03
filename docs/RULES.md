# Regras

Uma **regra** recebe o estado de um agente (`pulse`) e devolve uma decisão. Há três tipos, que convivem na mesma
pasta e participam do mesmo leilão de prioridade:

| Tipo | Arquivo | Para quê |
|---|---|---|
| **WebAssembly** | `*.wasm` | Regras em Rust/C/Go/AssemblyScript…, rápidas, determinísticas e isoladas. **Recomendado para autores não confiáveis (tenants)** |
| **Lua** *(legado)* | `*.lua` | Compatível com todas as regras existentes. Para autores confiáveis |
| **IA** | `*.rule.json` (`type: model` / `prompt`) | Modelos ONNX como regra, ou regras escritas em linguagem natural |

## Onde ficam

```
rules/default/*          fallback para todos os agentes                     (tenant "default")
rules/<agent_id>/*       regras de um agente — SUBSTITUEM as de default/ para ele
tenants/<id>/rules/...   o mesmo layout para outros tenants
```

Dentro de uma pasta, as regras rodam por **ordem de nome de arquivo**. Um arquivo `<nome>.rule.json` ao lado de
uma regra Lua/WASM é um *manifesto opcional* (veja [Manifestos](#manifestos)). Regras são compiladas **uma vez**
ao carregar (hot-reload automático ao salvar arquivos, ou `open-cardinal reload`); uma regra inválida é reportada
(`open-cardinal rules issues`, `/v1/rules`) e as demais continuam funcionando.

## O contrato

Entrada (`pulse` em Lua, JSON no WASM):

```json
{ "agent_id": "Rocket_01", "tenant": "default", "timestamp": 1700000000,
  "trace_id": "9f2c…", "telemetry": { "fuel": "53", "altitude": "120" } }
```

Saída (a mesma da wiki):

```json
{ "action": "SHUTDOWN", "cmd_name": "EMERGENCY_CUTOFF", "priority": 1000, "params": { "reason": "Overheating" } }
```

* `action`: `IDLE`, `SHUTDOWN`, `RESTART` ou `CUSTOM`. Qualquer outro valor é **erro da regra** (antes virava
  `IDLE` em silêncio, e uma regra com typo e prioridade alta podia "ganhar" com um IDLE).
* `params` aceita valores numéricos/booleanos (viram string, que é o tipo do protocolo).
* Retornar `nil` / `0` = "sem opinião".

## Leilão de prioridade

1. Regras determinísticas (Lua/WASM) rodam primeiro, em ordem de nome; as de IA por último.
2. Vence a **maior prioridade**; em empate, a **primeira** na ordem. Prioridade negativa nunca vence.
3. Uma decisão com prioridade **≥ 1000** é final: as regras restantes (inclusive IA) nem rodam.
4. `priority_cap` (manifesto) limita a prioridade de uma regra. **Regras de IA são limitadas a `ai.max_priority`
   do tenant (padrão 500)**: nenhuma IA supera uma regra determinística de emergência.
5. Se nenhuma regra decide, a resposta é `IDLE` com `command_name = "NO_ACTION"` (`"DIR_NOT_FOUND"` se não há
   pasta de regras para o agente).

## Memória compartilhada

Disponível em Lua (`redb_api`) e WASM (`cardinal.kv_*`). As chaves são **por tenant** (duas tenants podem usar
`"count"` sem se enxergar). Valores são inteiros ≥ 0 (`u64`). Com Raft, é o **mesmo estado em todos os nós**, e
as leituras são **linearizáveis** por padrão (veja [Cluster](CLUSTER.md#consistência)).

Limites: chave ≤ 256 bytes; no máximo `kv_max_writes_per_eval` (64) escritas por avaliação; `kv_max_entries`
chaves por tenant.

---

## Lua (legado)

Tudo o que a wiki documenta continua válido. O que mudou é o **ambiente de execução**:

* Bibliotecas disponíveis: base, `table`, `string`, `math`, `utf8`. **Removidas:** `os`, `io`, `package`, `debug`,
  `coroutine`, `dofile`, `loadfile`, `load`, `require`, `collectgarbage`, `print`.
  (Antes qualquer arquivo de regra executava `os.execute` — inclusive **ao ser salvo**.)
* Cada avaliação tem **orçamento de instruções** (`lua_instructions`), **limite de memória** (`lua_memory_bytes`)
  e **prazo** (`rule_timeout_ms`). Um `while true do end` termina com erro de orçamento em vez de travar o daemon.
* `redb_api`:
  * `redb_api.get(key)` → inteiro ou **`nil`** (antes dava *panic* se a chave não existia — o exemplo da wiki
    quebrava na primeira execução)
  * `redb_api.set(key, valor)`
  * `redb_api.incr(key [, delta=1])` → novo valor; **atômico e seguro em cluster** (use no lugar de `get`+`set`)
  * `redb_api.del(key)` → `true` se existia
* `cardinal.log(msg)` — escreve no log do daemon com contexto de tenant/agente.
* `math.random` é determinístico por `(agent_id, timestamp)`.
* Tenants novos têm Lua **desligado** por padrão (`rules.lua: true` para habilitar): o ambiente é restrito, mas
  uma única chamada nativa (casamento de padrões) ainda pode superar o gancho de instruções. Autores não
  confiáveis devem usar WASM.

```lua
-- 3 leituras quentes consecutivas => SHUTDOWN (contador compartilhado entre os nós do cluster)
local temp = tonumber(pulse.telemetry["cpu_temp"]) or 0
if temp > 90 then
    local n = redb_api.incr(pulse.agent_id .. "_strikes")
    if n >= 3 then
        redb_api.set(pulse.agent_id .. "_strikes", 0)
        return { action = "SHUTDOWN", cmd_name = "PERSISTENT_OVERHEAT", priority = 1000 }
    end
elseif (redb_api.get(pulse.agent_id .. "_strikes") or 0) > 0 then
    redb_api.set(pulse.agent_id .. "_strikes", 0)
end
return nil
```

Mais exemplos em [`examples/rules`](../examples/rules).

---

## WebAssembly

Interpretador [wasmi](https://github.com/wasmi-labs/wasmi): sem WASI (sem arquivos, rede, relógio ou
aleatoriedade), **cada instrução custa *fuel*** (`wasm_fuel`), memória limitada (`wasm_memory_bytes`), sem função
`start`, e o resultado só depende da entrada — o que combina com "supervisor determinístico".

### ABI

O módulo **exporta**:

| Símbolo | Assinatura | |
|---|---|---|
| `memory` | memória linear | |
| `cardinal_alloc` | `(size: i32) -> i32` | ponteiro para `size` bytes graváveis |
| `cardinal_evaluate` | `(ptr: i32, len: i32) -> i64` | recebe o JSON do pulse; devolve `(out_ptr << 32) \| out_len` apontando para o JSON da decisão, ou `0` = sem opinião |

E pode **importar** (módulo `cardinal`; qualquer outro import é recusado ao carregar):

| Função | |
|---|---|
| `kv_get(key_ptr, key_len, out_ptr) -> i32` | grava o `u64` (LE) em `out_ptr`; 1 = achou, 0 = ausente |
| `kv_set(key_ptr, key_len, value: i64) -> i32` | 0 = ok |
| `kv_add(key_ptr, key_len, delta: i64, out_ptr) -> i32` | soma atômica; grava o novo valor |
| `kv_del(key_ptr, key_len) -> i32` | 1 = removeu, 0 = ausente |
| `log(ptr, len)` | linha de diagnóstico (até 16 por avaliação) |

Códigos negativos: `-1` ponteiro inválido, `-2` orçamento/cota, `-3` erro de armazenamento, `-4` chave inválida.

Um exemplo completo em WAT (o gêmeo WASM da regra padrão) está em
[`examples/rules/wasm/low_fuel.wat`](../examples/rules/wasm/low_fuel.wat). Para montar:
`wat2wasm low_fuel.wat -o low_fuel.wasm` e salve como `rules/<agente>/low_fuel.wasm`.

> Autores em Rust: compile com `--target wasm32-unknown-unknown`, exporte `cardinal_alloc`/`cardinal_evaluate`
> e use `serde_json` para ler/escrever o JSON. (Um SDK para isso é um bom próximo passo — veja
> `docs/EDITIONS.md`.)

---

## IA (ONNX)

Compile com `--features onnx` (modelos) ou `--features prompt` (também regras em linguagem natural).
O ONNX Runtime é baixado e embarcado no build.

Princípios:

* **Modelos são registrados pelo operador** em `config/models.json` (caminho confinado a `models/`, `sha256`
  opcional para fixar o arquivo). Tenants só *referenciam* um modelo por nome, e só os que a política deles
  permite (`ai.models`). Um tenant nunca consegue fazer o daemon carregar um arquivo qualquer.
* **A IA escolhe de um cardápio que um humano escreveu.** Nunca inventa ações, comandos ou parâmetros.
* **A IA nunca supera regras determinísticas** (`ai.max_priority`).
* **Tudo é auditado**: a decisão carrega evidência (modelo, hash do arquivo, scores, hash do prompt).
* Erro, timeout ou valor inválido → "sem opinião".

```json
// config/models.json
{ "models": [
  { "name": "anomaly-v1", "path": "anomaly-v1.onnx", "sha256": "…", "intra_threads": 1, "pool": 2 },
  { "name": "qwen-small", "path": "qwen/model.onnx", "tokenizer": "qwen/tokenizer.json",
    "chat_template": "chatml", "max_context": 1024 } ] }
```

```json
// tenants.json — política de IA do tenant (desligada por padrão)
{ "id": "acme", "ai": { "enabled": true, "models": ["anomaly-v1"], "max_priority": 500 } }
```

### Modelo como regra (`type: "model"`)

Telemetria → vetor de *features* → modelo (`float32 [1, n]` → `float32`) → reação.

```json
{ "type": "model", "model": "anomaly-v1",
  "features": [ { "key": "cpu_temp", "default": 0 }, { "key": "fuel", "scale": 0.01 } ],
  "decide": { "mode": "threshold", "bands": [
      { "ge": 0.9, "action": "SHUTDOWN", "cmd_name": "AI_ANOMALY", "priority": 400 },
      { "ge": 0.6, "action": "RESTART",  "priority": 200 } ] } }
```

* `features[]`: `key`, `default` (sem ele, chave ausente é erro), `scale`, `offset`. Valores como `"80%"` são aceitos.
* `decide.mode = "threshold"`: `index` da saída + faixas `ge`; `"argmax"`: `classes[]` (uma reação por classe),
  `softmax`, `min_confidence`.
* Valida no carregamento: forma da entrada do modelo × número de features, tipos, ações.

### Regra em linguagem natural (`type: "prompt"`)

```json
{ "type": "prompt", "model": "qwen-small",
  "prompt": "Desligue o agente se a temperatura do motor passar de 90 graus; reinicie se a vibração passar de 7.",
  "include_telemetry": ["engine_temp", "vibration"],
  "choices": [
    { "label": "ok",       "action": "IDLE" },
    { "label": "restart",  "action": "RESTART",  "cmd_name": "AI_RESTART", "priority": 300 },
    { "label": "shutdown", "action": "SHUTDOWN", "cmd_name": "AI_CUTOFF",  "priority": 400 } ],
  "default": "ok", "min_confidence": 0.6 }
```

O modelo de linguagem recebe o prompt + a telemetria e o daemon calcula a **verossimilhança de cada rótulo**
(`choices[].label`) como continuação; o rótulo vencedor aponta para uma reação pré-autorizada. Não há texto livre
para interpretar.

* **Injeção de prompt:** valores de telemetria só entram no prompt se forem *tokens curtos* (números,
  identificadores; sem espaços). Texto livre vindo de um agente é substituído por `<non-numeric>`.
* Formato de modelo suportado: LM causal exportado **sem** `past_key_values`, com `input_ids` (int64) e
  opcionalmente `attention_mask`/`position_ids`, saída `logits` float32. Prefira rótulos de **um token** (uma
  única passada por pulse); com rótulos multi-token há uma passada por escolha.
* `prompt_file` (arquivo na própria pasta da regra) em vez de `prompt` inline.
* Validação: o conjunto é validado com um modelo sintético em CI. **Teste com o seu modelo antes de produção** —
  qualidade e latência dependem do modelo (aumente `ai_timeout_ms` do tenant para modelos maiores).

---

## Manifestos

`<nome>.rule.json` ao lado de `<nome>.lua` / `<nome>.wasm`:

```json
{ "enabled": true, "priority_cap": 600, "timeout_ms": 50 }
```

`enabled: false` desliga a regra sem apagar o arquivo; `timeout_ms` só pode *reduzir* o orçamento do tenant.
Para regras de IA, o manifesto **é** a regra (campo `type`).
