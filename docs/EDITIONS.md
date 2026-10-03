# Edições: open core e Enterprise/SaaS

> Este documento é uma **proposta** de estratégia de produto, escrita para orientar o código. As decisões de
> preço, licenciamento e empacotamento são suas; aqui está o que o código já oferece para sustentá-las.

## Princípio

* O **código aberto** (MIT) deve ser **bom o bastante para uma empresa colocar em produção** e se sentir segura
  fazendo isso. Se o open core for fraco, ninguém chega ao Enterprise; se for "capado" de má-fé, perde a
  comunidade.
* O **Enterprise/SaaS** vende o que empresas pagam para **não ter que operar**: governança, integração
  corporativa, escala multi-região, suporte e garantias — não funcionalidades de segurança básicas.
* Tudo que foi pedido para o código aberto está nele: **Raft** (memória compartilhada em cluster), **IA/ONNX**,
  **WebAssembly**, **multi-tenant**, **auditoria** com cadeia de hash, **TLS/mTLS**, **autenticação**, **Prometheus**,
  deploy em Docker/Swarm/Kubernetes. Isso é deliberado: quando o SaaS chegar, as expectativas já estarão altas.

## Divisão proposta

| Área | Open (MIT) | Enterprise / SaaS |
|---|---|---|
| Motor de regras | Lua (legado), WASM, modelos ONNX, regras por prompt, hot-reload, limites | SDKs oficiais (Rust/TS/Go/AssemblyScript), **registro de regras versionado** (revisão, aprovação, *rollback*), **simulador "e se?"** (re-executa pulses históricos contra uma regra nova antes de publicar), regras como *policy-as-code* com pipeline de CI |
| IA | Registro de modelos, classificadores, regras por prompt, teto de prioridade, evidência | **Catálogo curado de modelos** e *fine-tuning* gerenciado, avaliação contínua / detecção de *drift*, aceleradores (CUDA/TensorRT/DirectML) empacotados, quotas de GPU por tenant |
| Multi-tenant | Isolamento lógico, chaves, limites, auditoria por tenant | **SSO/OIDC/SAML + RBAC** (papéis, equipes, escopos), **faturamento e medição** por tenant, cotas e *chargeback*, tenants gerenciados por API/console, domínios de isolamento (um daemon dedicado por tenant) |
| Cluster | Raft em N hosts, descoberta por IP, join/remove, snapshots, mTLS | **Federação multi-região** (replicação assíncrona entre clusters), *learners* geo-distribuídos, **operador Kubernetes** (reconcilia membership/IPs, substitui nós mortos), upgrades orquestrados |
| Auditoria | Cadeia HMAC, consulta/exporta/verifica, `AuditSink` (trait) | **Retenção de longo prazo e WORM** (S3 Object Lock), conectores prontos (Splunk, Datadog, Sentinel, Elastic, Kafka), relatórios de conformidade (SOC 2, ISO 27001, IEC 62443), assinatura/âncora externa da cadeia (carimbo de tempo) |
| Segurança | API keys, mTLS, sandbox, limites | **KMS/HSM** (chaves de auditoria e de cluster), rotação a quente de segredos/certificados, SCIM, política de rede gerenciada |
| Operação | Prometheus, `/readyz`, logs JSON, CLI | **Console web** (decisões ao vivo, regras, tenants, cluster), alertas e SLOs gerenciados, **SaaS hospedado** (planos, SLA, suporte 24×7) |

O critério de corte: *o que um time pequeno consegue operar sozinho fica no open core; o que exige integração
corporativa, escala geográfica, conformidade ou ser operado por nós é Enterprise.*

## Pontos de extensão

O código aberto expõe os "encaixes" para que um crate **fechado** (`cardinal-enterprise`) dependa de
`open-cardinal` como biblioteca, sem *fork*:

| Encaixe | Onde | Para quê |
|---|---|---|
| `ext::Extensions` | `open_cardinal::ext` | Registro único: `edition`, `audit_sinks`, `authenticators` |
| `ext::AuditSink` | trait | Recebe cada registro de auditoria **depois** de gravado e encadeado, em ordem, em lote (SIEM, WORM, Kafka). Falha/panic do sink nunca atrasa a trilha local |
| `ext::Authenticator` | trait | Aceita tokens que não são chaves de API (JWT/OIDC, certificado mTLS) e os mapeia a um tenant existente + escopo de agente. Vale para gRPC e HTTP |
| `engine::RuleBackend` / `AiRuleFactory` | traits | Novos tipos de regra (por ex. WASM com JIT, regras em DSL próprio) e novos executores de IA (CUDA) |
| `daemon::run_with(paths, ext)` | função | `main` do binário Enterprise |
| `App::build_with` | função | Para embutir/testar |

```rust
// crate fechado: cardinal-enterprise/src/main.rs
use std::sync::Arc;
use open_cardinal::{config::Paths, daemon, ext::Extensions};

#[tokio::main]
async fn main() {
    let mut ext = Extensions::new("enterprise");
    ext.audit_sinks.push(Arc::new(my_siem::SplunkSink::from_env()));
    ext.authenticators.push(Arc::new(my_sso::OidcAuthenticator::from_env()));
    daemon::run_with(Paths::resolve(None), ext).await.unwrap();
}
```

O `status` e `/v1/status` já reportam `edition` e quais *features* de compilação estão ligadas, de modo que
suporte e console saibam o que cada nó executa.

### O que ainda não existe (e seria o primeiro trabalho Enterprise)

* Um *hook* de licença/entitlements (`LicenseGate`) — propositalmente **não** incluído: gatear funcionalidades já
  presentes no open core por licença seria o oposto do princípio acima. Funcionalidades *novas* do Enterprise
  devem ser **adições** (novos crates/rotas), não condicionais no código aberto.
* Rotas HTTP adicionais por extensão (o `router` é público e pode ser *merged* por quem chama `http::router`;
  falta um registro formal).
* Hot-rotation de segredos e certificados (hoje exige reinício).

## Caminho sugerido

1. **Agora (aberto):** publicar o open core com as docs deste repositório, imagens de container e o SDK WASM em Rust.
2. **SaaS v0:** hospedar o próprio daemon multi-tenant (já suportado) por trás de um plano de controle mínimo
   (criação de tenant, emissão de chave, painel de auditoria) — o código de provisionamento é o primeiro módulo
   fechado.
3. **Enterprise v1:** SSO/RBAC, retenção longa/WORM, conectores SIEM, console.
4. **Enterprise v2:** federação multi-região e operador Kubernetes.

Licenciar o crate fechado com termos comerciais e manter o aberto em MIT. Considere uma
**CLA** para contribuições externas ao open core, para preservar a liberdade de relicenciar/combinar.
