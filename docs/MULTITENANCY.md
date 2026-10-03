# Multi-tenant

Um **tenant** é um espaço isolado dentro do mesmo daemon (e do mesmo cluster). Cada tenant tem as suas:

* **regras** (`tenants/<id>/rules/...`, mesmo layout de `rules/`),
* **memória compartilhada** (`redb_api` / `cardinal.kv_*`: as chaves são prefixadas pelo tenant),
* **overrides do Heathcliff**,
* **chaves de API**,
* **limites** de execução, de tráfego e de memória,
* **política** de backends (Lua, WASM, IA, modelos permitidos),
* **trilha de auditoria** (cada registro leva o `tenant`; uma chave só lê o próprio).

O layout original (`rules/default`, `rules/<agente>`) é o tenant embutido **`default`**: quem não usa
multi-tenancy não precisa mudar nada.

## Criando um tenant

```bash
open-cardinal tenant add acme
open-cardinal tenant issue-key acme --label fleet-eu --agent-prefix eu-
# → api_key: ck_…   (mostrada UMA vez; só o SHA-256 é guardado)
mkdir -p tenants/acme/rules/default && cp minha_regra.wasm tenants/acme/rules/default/
```

Os agentes passam a chave em cada chamada: metadado gRPC `authorization: Bearer ck_…`
(o cliente de teste: `client --api-key ck_… ping …`).

* `--agent-prefix eu-`: essa chave só pode agir como agentes cujo id começa com `eu-`. Duas frotas do mesmo
  tenant não conseguem se passar uma pela outra.
* Chaves revogadas (`tenant revoke-key acme fleet-eu`) e tenants desabilitados (`tenant disable acme`) deixam de
  funcionar imediatamente.
* **Emitir a primeira chave liga a autenticação** (no modo `auto`, daemon em loopback). Antes disso o daemon
  aceita chamadas anônimas como o tenant `default`, exatamente como antes.

## `config/tenants.json`

```json
{ "tenants": [
  { "id": "acme", "display_name": "Acme Foods", "enabled": true,
    "api_keys": [ { "label": "fleet-eu", "sha256": "<hex>", "agent_prefix": "eu-" } ],
    "limits": { "rate_limit_per_sec": 500, "rate_limit_burst": 1000, "kv_max_entries": 50000,
                "wasm_fuel": 5000000 },
    "rules": { "lua": false, "wasm": true },
    "ai": { "enabled": true, "models": ["anomaly-v1"], "max_priority": 500 } } ] }
```

* O arquivo é recarregado ao ser salvo. **Se ficar inválido, o registro anterior continua valendo**: um erro de
  digitação nunca desliga a autenticação.
* `id`: `a-z`, `0-9`, `_`, `-` (até 63) — vira nome de pasta e componente de chave.
* `limits`: qualquer subconjunto dos limites de [Configuração](CONFIGURATION.md#limites-limits-também-por-tenant).
* `rules.lua`: padrão **`false`** para tenants novos (`true` só para `default`); `rules.wasm`: `true`.
* `ai`: padrão desligado; veja [IA](RULES.md#ia-onnx).

## Garantias de isolamento

| | Como é garantido |
|---|---|
| Regras | Cada tenant só carrega de `tenants/<id>/rules`; nomes de pasta são validados (`[A-Za-z0-9._-]`, sem symlinks) |
| Memória | Chave real = `(tenant, chave)`; o tenant é fixado pelo daemon, não pela regra |
| Overrides | Chave real = `(tenant, agent)` |
| CPU / memória / tempo | Orçamentos por avaliação (instruções ou *fuel*, heap, relógio) |
| Vizinho barulhento | Limite de avaliações simultâneas **por tenant** + rate limit; sob saturação o tenant recebe `RESOURCE_EXHAUSTED` sem afetar os outros |
| Armazenamento | `kv_max_entries` por tenant, `kv_max_writes_per_eval` por avaliação |
| Auditoria / HTTP | Uma chave de tenant só lê os registros e as regras do próprio tenant (`403` ao pedir outro) |
| Modelos de IA | Só os da allow-list; arquivos nunca são escolhidos pelo tenant |

### Limitações conhecidas

* Todos os tenants compartilham o mesmo processo e o mesmo `redb`. O isolamento é lógico e de recursos; para
  isolamento de fronteira (compliance rígido), rode um daemon por tenant.
* Regras **Lua** não são adequadas a autores hostis (veja [Regras](RULES.md#lua-legado)). O padrão já reflete isso.
* O orçamento de relógio (`rule_timeout_ms`) é para *todas* as regras de um pulse; limites por tenant evitam que
  um tenant consuma a capacidade global, mas a soma dos limites de vários tenants pode exceder a CPU disponível.
